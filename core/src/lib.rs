//! Parse and compare weight Substrate weight files.

#![deny(rustdoc::broken_intra_doc_links)]

use clap::Args;
use fancy_regex::Regex;
use git_version::git_version;
use lazy_static::lazy_static;

use std::{
	cmp::Ordering,
	collections::{BTreeSet, HashMap, HashSet},
	path::{Path, PathBuf},
	process::Command,
};
use syn::{Expr, Item, Type};

pub mod parse;
pub mod scope;
pub mod term;
pub mod testing;
pub mod traits;

#[cfg(test)]
mod test;

use parse::pallet::{
	parse_files_in_repo, try_parse_files_in_repo, ChromaticExtrinsic, ComponentRange,
	SimpleExtrinsic,
};
use scope::SimpleScope;
use term::SimpleTerm;

lazy_static! {
	/// Version of the library. Example: `swc 0.2.0+78a04b2-dirty`.
	pub static ref VERSION: String = format!("{}+{}", env!("CARGO_PKG_VERSION"), git_version!(args = ["--dirty", "--always"], fallback = "unknown"));

	pub static ref VERSION_DIRTY: bool = {
		VERSION.clone().contains("dirty")
	};
}

pub type PalletName = String;
pub type ExtrinsicName = String;
pub type TotalDiff = Vec<ExtrinsicDiff>;

pub type Percent = f64;
pub const WEIGHT_PER_NANOS: u128 = 1_000;

#[derive(Clone)]
#[cfg_attr(feature = "bloat", derive(Debug))]
pub struct ExtrinsicDiff {
	pub name: ExtrinsicName,
	pub file: String,

	pub change: TermDiff,
}

#[derive(Clone)]
#[cfg_attr(feature = "bloat", derive(Debug))]
pub enum TermDiff {
	Changed(TermChange),
	Warning(TermChange, String),
	Failed(String),
}

impl ExtrinsicDiff {
	pub fn term(&self) -> Option<&TermChange> {
		match &self.change {
			TermDiff::Changed(change) => Some(change),
			TermDiff::Warning(change, _) => Some(change),
			_ => None,
		}
	}

	pub fn error(&self) -> Option<&String> {
		match &self.change {
			TermDiff::Failed(err) => Some(err),
			_ => None,
		}
	}

	pub fn warning(&self) -> Option<&String> {
		match &self.change {
			TermDiff::Warning(_, warning) => Some(warning),
			_ => None,
		}
	}
}

// Uses options since extrinsics can be added or removed and any time.
#[derive(Clone)]
#[cfg_attr(feature = "bloat", derive(Debug))]
pub struct TermChange {
	pub old: Option<SimpleTerm>,
	pub old_v: Option<u128>,

	pub new: Option<SimpleTerm>,
	pub new_v: Option<u128>,

	pub scope: SimpleScope,
	pub percent: Percent,
	pub change: RelativeChange,
	pub method: CompareMethod,
}

// TODO rename
#[derive(
	Debug, serde::Deserialize, clap::ValueEnum, Clone, Eq, Ord, PartialEq, PartialOrd, Copy,
)]
#[serde(rename_all = "kebab-case")]
pub enum RelativeChange {
	Unchanged,
	Added,
	Removed,
	Changed,
}

/// Parameters for modifying the benchmark behaviour.
#[derive(Debug, Clone, PartialEq, Eq, Args)]
pub struct CompareParams {
	#[clap(long, short, value_name = "METHOD", ignore_case = true)]
	pub method: CompareMethod,

	#[clap(long, short, value_name = "UNIT", ignore_case = true, default_value = "time")]
	pub unit: Dimension,

	#[clap(long)]
	pub ignore_errors: bool,

	/// Do a 'git pull' after checking out the refname.
	///
	/// This ensures that you get the newest commit on a branch.
	#[clap(long)]
	pub git_pull: bool,

	/// Don't access the network.
	///
	/// This overrides any other options like `--git-pull`.
	#[clap(long)]
	pub offline: bool,
}

#[derive(Debug, Clone, PartialEq, Args)]
#[cfg_attr(feature = "bloat", derive(Default))]
pub struct FilterParams {
	/// Minimal magnitude of a relative change to be relevant.
	#[clap(long, value_name = "PERCENT", default_value = "5")]
	pub threshold: Percent,

	/// Only include a subset of change-types.
	#[clap(long, ignore_case = true, num_args = 0.., value_name = "CHANGE-TYPE")]
	pub change: Option<Vec<RelativeChange>>,

	#[clap(long, ignore_case = true, value_name = "REGEX")]
	pub extrinsic: Option<String>,

	#[clap(long, alias("file"), ignore_case = true, value_name = "REGEX")]
	pub pallet: Option<String>,
}

impl CompareParams {
	pub fn should_pull(&self) -> bool {
		self.git_pull && !self.offline
	}
}

pub fn compare_commits(
	repo: &Path,
	old: &str,
	new: &str,
	params: &CompareParams,
	filter: &FilterParams,
	path_pattern: &str,
	max_files: usize,
) -> Result<TotalDiff, Box<dyn std::error::Error>> {
	if path_pattern.contains("..") {
		return Err("Path pattern cannot contain '..'".into())
	}
	// Parse the old files.
	if let Err(err) = reset(repo, old, params.should_pull()) {
		return Err(format!("{:?}", err).into())
	}
	let paths = list_files(repo, path_pattern, max_files)?;
	// Ignore any parsing errors.
	let olds = if params.ignore_errors {
		try_parse_files_in_repo(repo, &paths)
	} else {
		// TODO use option for repo
		parse_files_in_repo(repo, &paths)?
	};

	// Parse the new files.
	if let Err(err) = reset(repo, new, params.should_pull()) {
		return Err(format!("{:?}", err).into())
	}
	let paths = list_files(repo, path_pattern, max_files)?;
	// Ignore any parsing errors.
	let news = if params.ignore_errors {
		try_parse_files_in_repo(repo, &paths)
	} else {
		parse_files_in_repo(repo, &paths)?
	};

	compare_files(olds, news, params, filter)
}

pub fn reset(path: &Path, refname: &str, pull: bool) -> Result<(), String> {
	if pull {
		log::info!("Fetching branch {}", refname);

		let output = Command::new("git")
			.arg("fetch")
			.arg("origin")
			.arg(refname)
			.current_dir(path)
			.output()
			.map_err(|e| format!("Failed to fetch branch: {:?}", &e))?;
		if !output.status.success() {
			return Err(format!(
				"Failed to fetch branch: {}",
				String::from_utf8_lossy(&output.stderr),
			))
		}
	} else {
		log::debug!("Not fetching branch {} (should_fetch={})", refname, pull);
	}
	// try to reset with remote...
	log::info!("Resetting to origin/{}", refname);
	let output = Command::new("git")
		.arg("reset")
		.arg("--hard")
		.arg(format!("origin/{}", refname))
		.current_dir(path)
		.output();
	// Ignore any errors and try again without `origin/` prefix.
	match output {
		Err(err) => log::info!("Failed to reset to origin/{}: {}", refname, err),
		Ok(output) =>
			if !output.status.success() {
				log::warn!("Failed to reset to: origin/{}", String::from_utf8_lossy(&output.stderr))
			} else {
				return Ok(())
			},
	}
	// Try resetting without remote.
	log::info!("Fallback: Resetting to {}", refname);
	let output = Command::new("git")
		.arg("reset")
		.arg("--hard")
		.arg(refname)
		.current_dir(path)
		.output()
		.map_err(|e| format!("Failed to reset branch: {:?}", e))?;

	if !output.status.success() {
		return Err(format!("Failed to reset branch: {}", String::from_utf8_lossy(&output.stderr)))
	}
	Ok(())
}

fn list_files(
	base_path: &Path,
	regex: &str,
	max_files: usize,
) -> Result<Vec<PathBuf>, Box<dyn std::error::Error>> {
	let regex = regex.split(',');

	let mut paths = Vec::new();
	for regex in regex {
		let regex = format!("{}/{}", base_path.display(), regex);
		log::info!("Listing files matching: {:?}", &regex);
		let files = glob::glob(&regex).map_err(|e| format!("Invalid path pattern: {:?}", e))?;
		let files = files
			.collect::<Result<Vec<_>, _>>()
			.map_err(|e| format!("Path pattern error: {:?}", e))?;
		let files: Vec<_> = files.iter().cloned().filter(|f| !f.ends_with("mod.rs")).collect();
		paths.extend(files);
		if paths.len() > max_files {
			return Err(
				format!("Found too many files. Found: {}, Max: {}", paths.len(), max_files).into()
			)
		}
	}
	paths.sort();
	paths.dedup();
	Ok(paths)
}

#[derive(serde::Deserialize, clap::ValueEnum, PartialEq, Eq, Hash, Clone, Copy, Debug)]
#[serde(rename_all = "kebab-case")]
pub enum CompareMethod {
	/// The constant base weight of the extrinsic.
	Base,

	/// Try to find the worst case increase. Errors if any component misses a range annotation.
	ExactWorst,
	/// Similar to [`Self::ExactWorst`], but guesses if any component misses a range annotation.
	GuessWorst,
	/// Set all components to their exact maximum value.
	Asymptotic,
}

impl CompareMethod {
	pub const fn min(&self) -> ComponentInstanceStrategy {
		match self {
			Self::Base | Self::GuessWorst => ComponentInstanceStrategy::guess_min(),
			Self::ExactWorst => ComponentInstanceStrategy::exact_min(),
			Self::Asymptotic => ComponentInstanceStrategy::exact_max(),
		}
	}

	pub const fn max(&self) -> ComponentInstanceStrategy {
		match self {
			Self::Base => ComponentInstanceStrategy::guess_min(),
			Self::GuessWorst => ComponentInstanceStrategy::guess_max(),
			Self::ExactWorst | Self::Asymptotic => ComponentInstanceStrategy::exact_max(),
		}
	}
}

#[derive(PartialEq, Eq, Hash, Clone, Copy, Debug)]
pub struct ComponentInstanceStrategy {
	pub exact: bool,
	pub min_or_max: MinOrMax,
}

impl ComponentInstanceStrategy {
	pub const fn exact_min() -> Self {
		Self { exact: true, min_or_max: MinOrMax::Min }
	}

	pub const fn exact_max() -> Self {
		Self { exact: true, min_or_max: MinOrMax::Max }
	}

	pub const fn guess_min() -> Self {
		Self { exact: false, min_or_max: MinOrMax::Min }
	}

	pub const fn guess_max() -> Self {
		Self { exact: false, min_or_max: MinOrMax::Max }
	}
}

#[derive(serde::Deserialize, clap::ValueEnum, PartialEq, Eq, Hash, Clone, Copy, Debug)]
pub enum MinOrMax {
	Min,
	Max,
}

impl core::fmt::Display for MinOrMax {
	fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
		match self {
			MinOrMax::Min => write!(f, "min"),
			MinOrMax::Max => write!(f, "max"),
		}
	}
}

// We call this *Unit* for ease of use but it is actually a *dimension* and a unit.
#[derive(serde::Deserialize, clap::ValueEnum, PartialEq, Eq, Hash, Clone, Copy, Debug)]
#[serde(rename_all = "kebab-case")]
pub enum Dimension {
	/// Reference time. Alias to `weight` for backwards compatibility.
	#[serde(alias = "weight")]
	Time,

	/// Proof-of-validity (PoV) size.
	Proof,
}

impl std::str::FromStr for CompareMethod {
	type Err = String;

	fn from_str(s: &str) -> Result<Self, String> {
		match s {
			"base" => Ok(CompareMethod::Base),
			"guess-worst" => Ok(CompareMethod::GuessWorst),
			"exact-worst" => Ok(CompareMethod::ExactWorst),
			"asymptotic" => Ok(CompareMethod::Asymptotic),
			_ => Err(format!("Unknown method: {}", s)),
		}
	}
}

impl CompareMethod {
	pub fn all() -> Vec<Self> {
		vec![Self::Base, Self::GuessWorst, Self::ExactWorst, Self::Asymptotic]
	}

	pub fn variants() -> Vec<&'static str> {
		vec!["base", "guess-worst", "exact-worst", "asymptotic"]
	}

	pub fn reflect() -> Vec<(Self, &'static str)> {
		Self::all().into_iter().zip(Self::variants().into_iter()).collect()
	}
}

impl std::str::FromStr for Dimension {
	type Err = String;

	fn from_str(s: &str) -> Result<Self, String> {
		match s {
			"time" | "weight" => Ok(Self::Time),
			"proof" => Ok(Self::Proof),
			_ => Err(format!("Unknown method: {}", s)),
		}
	}
}

impl FilterParams {
	pub fn included(&self, change: &RelativeChange) -> bool {
		self.change.as_ref().map_or(true, |s| s.contains(change))
	}
}

impl std::str::FromStr for RelativeChange {
	type Err = String;
	// TODO try clap ValueEnum
	fn from_str(s: &str) -> Result<Self, String> {
		match s {
			"unchanged" => Ok(Self::Unchanged),
			"changed" => Ok(Self::Changed),
			"added" => Ok(Self::Added),
			"removed" => Ok(Self::Removed),
			_ => Err(format!("Unknown change: {}", s)),
		}
	}
}

impl RelativeChange {
	pub fn variants() -> Vec<&'static str> {
		vec!["unchanged", "changed", "added", "removed"]
	}
}

pub fn compare_extrinsics(
	mut old: Option<SimpleExtrinsic>,
	mut new: Option<SimpleExtrinsic>,
	params: &CompareParams,
) -> Result<TermChange, String> {
	let mut scope = scope::SimpleScope::empty();
	if params.unit == Dimension::Time {
		scope = scope
			.with_storage_weights(SimpleTerm::Scalar(25_000_000), SimpleTerm::Scalar(100_000_000));
	} else {
		scope = scope.with_storage_weights(SimpleTerm::Scalar(0), SimpleTerm::Scalar(0));
		// OMG this code is stupid... but since READ and WRITE done incur proof size cost, we ignore
		// them.
		old = old.map(|mut o| {
			o.term.substitute("READ", &scalar!(0));
			o
		});
		old = old.map(|mut o| {
			o.term.substitute("WRITE", &scalar!(0));
			o
		});
		new = new.map(|mut o| {
			o.term.substitute("READ", &scalar!(0));
			o
		});
		new = new.map(|mut o| {
			o.term.substitute("WRITE", &scalar!(0));
			o
		});
	}
	let (new, old) = (new.as_ref(), old.as_ref());
	let scopes = extend_scoped_components(old, new, params.method, &scope)?;
	let name = old.map(|o| o.name.clone()).or_else(|| new.map(|n| n.name.clone())).unwrap();
	let pallet = old.map(|o| o.pallet.clone()).or_else(|| new.map(|n| n.pallet.clone())).unwrap();

	let mut results = Vec::<TermChange>::new();

	for scope in scopes.iter() {
		if !old.map_or(true, |e| e.term.free_vars(scope).is_empty()) {
			unreachable!(
				"Free variable where there should be none: {}::{} {:?}",
				name,
				&pallet,
				old.unwrap().term.free_vars(scope)
			);
		}
		assert!(new.map_or(true, |e| e.term.free_vars(scope).is_empty()));
		// NOTE: The maximum could be calculated right here, but for now I want the debug assert.
		results.push(compare_terms(
			old.map(|o| &o.term),
			new.map(|n| &n.term),
			params.method,
			scope,
		)?);
	}
	log::trace!(target: "compare", "{}::{} Evaluated {} scopes", pallet, name, scopes.len());

	// Sanity check: They are either
	// All Increase/Decrease/Unchanged OR
	// All Added/Removed
	let all_increase_or_decrease = results
		.iter()
		.all(|r| matches!(r.change, RelativeChange::Changed | RelativeChange::Unchanged));
	let all_added_or_removed = results
		.iter()
		.all(|r| matches!(r.change, RelativeChange::Added | RelativeChange::Removed));

	if all_added_or_removed {
		// Just pick the first one
		Ok(results.into_iter().next().unwrap())
	} else if all_increase_or_decrease {
		Ok(results.into_iter().max_by(|a, b| a.cmp(b)).unwrap())
	} else {
		unreachable!(
			"Inconclusive: all_increase_or_decrease: {}, all_added_or_removed: {}",
			all_increase_or_decrease, all_added_or_removed
		);
	}
}

// TODO handle case that both have (different) ranges.
pub(crate) fn extend_scoped_components(
	a: Option<&SimpleExtrinsic>,
	b: Option<&SimpleExtrinsic>,
	method: CompareMethod,
	scope: &SimpleScope,
) -> Result<Vec<SimpleScope>, String> {
	let free_a = a.map(|e| e.term.free_vars(scope)).unwrap_or_default();
	let free_b = b.map(|e| e.term.free_vars(scope)).unwrap_or_default();
	let frees = free_a.union(&free_b).cloned().collect::<HashSet<_>>();

	let ra = a.map(|ext| ext.clone().comp_ranges.unwrap_or_default());
	let rb = b.map(|ext| ext.clone().comp_ranges.unwrap_or_default());

	let (pallet, extrinsic) = a.or(b).map(|e| (e.pallet.clone(), e.name.clone())).unwrap();

	if frees.len() > 16 {
		return Err(format!(
			"Too many components to compare: {}::{} has {} components - limit is 16",
			pallet,
			extrinsic,
			frees.len()
		))
	}
	// Combine the maximum and minimum of each component with combinatorics.
	let (mut lowest, mut highest) = (Vec::new(), Vec::new());
	for free in frees.iter() {
		lowest.push(instance_component(free, &ra, &rb, method.min(), &pallet, &extrinsic)?);
		highest.push(instance_component(free, &ra, &rb, method.max(), &pallet, &extrinsic)?);
	}

	// cartesian product of lowest and highest
	let mut scopes = BTreeSet::new();
	for i in 0..(1 << frees.len()) {
		let mut scope = scope.clone();
		for (c, component) in frees.iter().enumerate() {
			let value = if i & (1 << c) == 0 { lowest[c] } else { highest[c] };
			scope.put_var(component, SimpleTerm::Scalar(value as u128));
		}
		if !scope.is_empty() {
			scopes.insert(scope);
		}
	}
	Ok(scopes.into_iter().collect())
}

fn instance_component(
	component: &str,
	ra: &Option<HashMap<String, ComponentRange>>,
	rb: &Option<HashMap<String, ComponentRange>>,
	strategy: ComponentInstanceStrategy,
	pallet: &str,
	extrinsic: &str,
) -> Result<u32, String> {
	use MinOrMax::*;

	match (ra.as_ref().and_then(|r| r.get(component)), rb.as_ref().and_then(|r| r.get(component))) {
		// Only one extrinsic has a component range? Good
		(Some(r), None) | (None, Some(r)) => Ok(match strategy.min_or_max {
			Min => r.min,
			Max => r.max,
		}),
		// Both extrinsics have the same range? Good
		(Some(ra), Some(rb)) if ra == rb => Ok(match strategy.min_or_max {
			Min => ra.min,
			Max => ra.max,
		}),
		// Both extrinsics have different ranges? Bad, use the min/max.
		(Some(ra), Some(rb)) => match (strategy.exact, strategy.min_or_max) {
			(true, _) => Err(format!(
				"Component {} of call {}::{} has different ranges in the old and new version - Use Guess instead!",
				component, pallet, extrinsic,
			)),
			(false, Min) => Ok(ra.min.min(rb.min)),
			(false, Max) => Ok(ra.max.max(rb.max)),
		},
		// No ranges? Bad, just guess 100.
		(None, None) => match (strategy.exact, strategy.min_or_max) {
			(false, Min) => Ok(0),
			(false, Max) => Ok(100),
			(true, _) => Err(format!(
				"No range for component {} of call {}::{} - use Guess instead!",
				component, pallet, extrinsic,
			)),
		},
	}
}

pub fn compare_terms(
	old: Option<&SimpleTerm>,
	new: Option<&SimpleTerm>,
	method: CompareMethod,
	scope: &SimpleScope,
) -> Result<TermChange, String> {
	let old_v = old.map(|t| t.eval(scope)).transpose()?;
	let new_v = new.map(|t| t.eval(scope)).transpose()?;
	let change =
		if old == new { RelativeChange::Unchanged } else { RelativeChange::new(old_v, new_v) };
	let p = percent(old_v.unwrap_or_default(), new_v.unwrap_or_default());
	log::trace!(target: "compare", "Evaluating {:?}  vs {:?} ({:?}) [{:?}]", old_v.unwrap_or_default(), new_v.unwrap_or_default(), p, &scope);

	Ok(TermChange {
		old: old.cloned(),
		old_v,
		new: new.cloned(),
		new_v,
		change,
		percent: p,
		method,
		scope: scope.clone(),
	})
}

pub fn compare_files(
	olds: Vec<ChromaticExtrinsic>,
	news: Vec<ChromaticExtrinsic>,
	params: &CompareParams,
	filter: &FilterParams,
) -> Result<TotalDiff, Box<dyn std::error::Error>> {
	let ext_regex = filter.extrinsic.as_ref().map(|s| Regex::new(s)).transpose()?;
	let pallet_regex = filter.pallet.as_ref().map(|s| Regex::new(s)).transpose()?;
	// Split them into their correct dimension.
	let olds = olds
		.into_iter()
		.map(|e| e.map_term(|t| t.simplify(params.unit).expect("Must simplify term")))
		.collect::<Vec<_>>();
	let news = news
		.into_iter()
		.map(|e| e.map_term(|t| t.simplify(params.unit).expect("Must simplify term")))
		.collect::<Vec<_>>();

	let mut diff = TotalDiff::new();
	let old_names = olds.iter().cloned().map(|e| (e.pallet, e.name));
	let new_names = news.iter().cloned().map(|e| (e.pallet, e.name));
	let names = old_names.chain(new_names).collect::<std::collections::BTreeSet<_>>();
	log::trace!("Comparing {} terms", olds.len());

	for (pallet, extrinsic) in names {
		if !pallet_regex.as_ref().map_or(true, |r| r.is_match(&pallet).unwrap_or_default()) {
			// TODO add "skipped" or "ignored" result type.
			continue
		}
		if !ext_regex.as_ref().map_or(true, |r| r.is_match(&extrinsic).unwrap_or_default()) {
			continue
		}

		let new = news.iter().find(|&n| n.name == extrinsic && n.pallet == pallet);
		let old = olds.iter().find(|&n| n.name == extrinsic && n.pallet == pallet);
		log::trace!("Comparing {}::{}", pallet, extrinsic);

		let change = match compare_extrinsics(old.cloned(), new.cloned(), params) {
			Err(err) => {
				log::warn!("Parsing failed {}: {:?}", &pallet, err);
				TermDiff::Failed(err)
			},
			Ok(change) =>
				if let Some(ext) = new.or(old) {
					if let Err(err) = sanity_check_term(&ext.term)
						.map_err(|e| format!("{}: {}::{}", e, ext.pallet, ext.name))
					{
						TermDiff::Warning(change, err)
					} else {
						TermDiff::Changed(change)
					}
				} else {
					unreachable!(
						"We already checked that the extrinsic exists in either old or new"
					)
				},
		};

		diff.push(ExtrinsicDiff { name: extrinsic.clone(), file: pallet.clone(), change });
	}

	Ok(diff)
}

/// Checks some obvious stuff:
/// - Does not have more than 1000 reads or writes
pub fn sanity_check_term(term: &SimpleTerm) -> Result<(), String> {
	let reads = term.find_largest_factor("READ").unwrap_or_default();
	let writes = term.find_largest_factor("WRITE").unwrap_or_default();
	let larger = reads.max(writes);

	if larger > 1000 {
		if reads > writes {
			Err(format!("Call has {} READs", reads))
		} else {
			Err(format!("Call has {} WRITEs", writes))
		}
	} else {
		Ok(())
	}
}

pub fn sort_changes(diff: &mut TotalDiff) {
	diff.sort_by(|a, b| a.change.cmp(&b.change));
}

impl TermDiff {
	fn cmp(&self, other: &Self) -> Ordering {
		match (&self, &other) {
			(TermDiff::Failed(_), _) => Ordering::Less,
			(_, TermDiff::Failed(_)) => Ordering::Greater,
			(TermDiff::Warning(a, _), TermDiff::Changed(b)) => a.cmp(b),
			(TermDiff::Changed(a), TermDiff::Warning(b, _)) => a.cmp(b),
			(TermDiff::Warning(a, _), TermDiff::Warning(b, _)) => a.cmp(b),
			(TermDiff::Changed(a), TermDiff::Changed(b)) => a.cmp(b),
		}
	}
}

impl TermChange {
	fn cmp(&self, other: &Self) -> Ordering {
		let ord = self.change.cmp(&other.change);
		if ord == Ordering::Equal {
			/*if self.percent > other.percent {
				Ordering::Greater
			} else if self.percent == other.percent {
				Ordering::Equal
			} else {
				Ordering::Less
			}*/
			((self.percent * 1000.0) as i128).cmp(&((other.percent * 1000.0) as i128))
		} else {
			ord
		}
	}
}

pub fn filter_changes(diff: TotalDiff, params: &FilterParams) -> TotalDiff {
	// Note: the pallet and extrinsic are already filtered in compare_files.
	diff.iter()
		.filter(|extrinsic| match extrinsic.change {
			TermDiff::Failed(_) => true,
			TermDiff::Warning(ref change, ..) | TermDiff::Changed(ref change) => {
				if !params.included(&change.change) {
					return false
				}

				match change.change {
					RelativeChange::Changed if change.percent.abs() < params.threshold => false,
					RelativeChange::Unchanged if params.threshold >= 0.000001 => false,
					_ => true,
				}
			},
		})
		.cloned()
		.collect()
}

impl RelativeChange {
	pub fn new(old: Option<u128>, new: Option<u128>) -> RelativeChange {
		match (old, new) {
			//(old, new) if old == new => RelativeChange::Unchanged,
			(Some(_), Some(_)) => RelativeChange::Changed,
			(None, Some(_)) => RelativeChange::Added,
			(Some(_), None) => RelativeChange::Removed,
			(None, None) => unreachable!("Either old or new must be set"),
		}
	}
}

pub fn percent(old: u128, new: u128) -> Percent {
	100.0 * (new as f64 / old as f64) - 100.0
}

impl Dimension {
	pub fn fmt_value(&self, v: u128) -> String {
		match self {
			Self::Time => Self::fmt_time(v),
			Self::Proof => Self::fmt_proof(v),
		}
	}

	pub fn fmt_scalar(w: u128) -> String {
		if w >= 1_000_000_000_000 {
			format!("{:.2}T", w as f64 / 1_000_000_000_000f64)
		} else if w >= 1_000_000_000 {
			format!("{:.2}G", w as f64 / 1_000_000_000f64)
		} else if w >= 1_000_000 {
			format!("{:.2}M", w as f64 / 1_000_000f64)
		} else if w >= 1_000 {
			format!("{:.2}K", w as f64 / 1_000f64)
		} else {
			w.to_string()
		}
	}

	/// Formats pico seconds.
	pub fn fmt_time(t: u128) -> String {
		if t >= 1_000_000_000_000 {
			format!("{:.2}s", t as f64 / 1_000_000_000_000f64)
		} else if t >= 1_000_000_000 {
			format!("{:.2}ms", t as f64 / 1_000_000_000f64)
		} else if t >= 1_000_000 {
			format!("{:.2}us", t as f64 / 1_000_000f64)
		} else if t >= 1_000 {
			format!("{:.2}ns", t as f64 / 1_000f64)
		} else {
			format!("{:.2}ps", t)
		}
	}

	pub fn fmt_proof(b: u128) -> String {
		const BYTE_PER_KIB: u128 = 1024;
		const BYTE_PER_MIB: u128 = BYTE_PER_KIB * 1024;
		const BYTE_PER_GIB: u128 = BYTE_PER_MIB * 1024;

		if b >= BYTE_PER_GIB {
			format!("{:.2}GiB", b as f64 / BYTE_PER_GIB as f64)
		} else if b >= BYTE_PER_MIB {
			format!("{:.2}MiB", b as f64 / BYTE_PER_MIB as f64)
		} else if b >= BYTE_PER_KIB {
			format!("{:.2}KiB", b as f64 / BYTE_PER_KIB as f64)
		} else {
			format!("{}B", b)
		}
	}

	pub fn all() -> Vec<Self> {
		vec![Self::Time, Self::Proof]
	}

	pub fn variants() -> Vec<&'static str> {
		vec!["time", "proof"]
	}

	pub fn reflect() -> Vec<(Self, &'static str)> {
		Self::all().into_iter().zip(Self::variants().into_iter()).collect()
	}
}
