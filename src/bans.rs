pub mod cfg;
mod diags;
mod graph;

use self::cfg::{TreeSkip, ValidConfig};
use crate::{
    diag::{self, CfgCoord, FileId, KrateCoord},
    Kid, Krate, Krates, LintLevel,
};
use anyhow::Error;
use semver::VersionReq;
use std::fmt;

#[derive(PartialEq)]
#[cfg_attr(test, derive(Debug))]
pub struct KrateId {
    pub(crate) name: String,
    pub(crate) version: Option<VersionReq>,
}

impl fmt::Display for KrateId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} = {:?}", self.name, self.version)
    }
}

struct ReqMatch<'vr> {
    id: &'vr cfg::Skrate,
    index: usize,
}

/// Returns the version requirements that matched the version, if any
#[inline]
fn matches<'v>(arr: &'v [cfg::Skrate], details: &Krate) -> Option<Vec<ReqMatch<'v>>> {
    let matches: Vec<_> = arr
        .iter()
        .enumerate()
        .filter_map(|(index, req)| {
            if req.value.name == details.name
                && crate::match_req(&details.version, req.value.version.as_ref())
            {
                Some(ReqMatch { id: req, index })
            } else {
                None
            }
        })
        .collect();

    if matches.is_empty() {
        None
    } else {
        Some(matches)
    }
}

struct SkipRoot {
    span: std::ops::Range<usize>,
    skip_crates: Vec<Kid>,
    skip_hits: bitvec::vec::BitVec,
}

use bitvec::prelude::*;

// If trees are being skipped, walk each one down to the specified depth and add
// each dependency as a skipped crate at the specific version
struct TreeSkipper {
    roots: Vec<SkipRoot>,
    cfg_file_id: FileId,
}

impl TreeSkipper {
    fn build(
        skip_roots: Vec<crate::Spanned<TreeSkip>>,
        krates: &Krates,
        cfg_file_id: FileId,
    ) -> (Self, Pack) {
        let mut roots = Vec::with_capacity(skip_roots.len());

        let mut pack = Pack::new(Check::Bans);

        for ts in skip_roots {
            let num_roots = roots.len();

            for krate in krates
                .krates_by_name(&ts.value.id.name)
                .filter(|(_index, node)| {
                    crate::match_req(&node.krate.version, ts.value.id.version.as_ref())
                })
            {
                roots.push(Self::build_skip_root(ts.clone(), krate.0, krates));
            }

            // If no roots were added, add a diagnostic that the user's configuration
            // is outdated so they can fix or clean it up
            if roots.len() == num_roots {
                pack.push(diags::UnmatchedSkipRoot {
                    skip_root_cfg: CfgCoord {
                        file: cfg_file_id,
                        span: ts.span,
                    },
                });
            }
        }

        (Self { roots, cfg_file_id }, pack)
    }

    fn build_skip_root(
        ts: crate::Spanned<TreeSkip>,
        krate_id: krates::NodeId,
        krates: &Krates,
    ) -> SkipRoot {
        let span = ts.span;
        let ts = ts.value;

        let max_depth = ts.depth.unwrap_or(std::usize::MAX);
        let mut skip_crates = Vec::with_capacity(10);

        let graph = krates.graph();

        let mut pending = vec![(krate_id, 1)];
        while let Some((node_id, depth)) = pending.pop() {
            let pkg_id = &krates[node_id].id;
            if let Err(i) = skip_crates.binary_search(pkg_id) {
                skip_crates.insert(i, pkg_id.clone());

                if depth < max_depth {
                    for dep in graph.edges_directed(node_id, Direction::Outgoing) {
                        pending.push((dep.target(), depth + 1));
                    }
                }
            }
        }

        let skip_hits = BitVec::repeat(false, skip_crates.len());

        SkipRoot {
            span,
            skip_crates,
            skip_hits,
        }
    }

    fn matches(&mut self, krate: &Krate, pack: &mut Pack) -> bool {
        let mut skip = false;

        for root in &mut self.roots {
            if let Ok(i) = root.skip_crates.binary_search(&krate.id) {
                pack.push(diags::SkippedByRoot {
                    krate,
                    skip_root_cfg: CfgCoord {
                        file: self.cfg_file_id,
                        span: root.span.clone(),
                    },
                });

                root.skip_hits.as_mut_bitslice().set(i, true);
                skip = true;
            }
        }

        skip
    }
}

pub struct DupGraph {
    pub duplicate: String,
    pub graph: String,
}

pub type OutputGraph = dyn Fn(DupGraph) -> Result<(), Error> + Send + Sync;

use crate::diag::{Check, Diag, Pack, Severity};
use krates::petgraph::{visit::EdgeRef, Direction};

pub fn check(
    ctx: crate::CheckCtx<'_, ValidConfig>,
    output_graph: Option<Box<OutputGraph>>,
    cargo_spans: diag::CargoSpans,
    mut sink: diag::ErrorSink,
) {
    let wildcard = VersionReq::parse("*").expect("Parsing wildcard mustnt fail");

    let ValidConfig {
        file_id,
        denied,
        allowed,
        skipped,
        multiple_versions,
        highlight,
        tree_skipped,
        wildcards,
        allow_build_scripts,
    } = ctx.cfg;

    let krate_spans = &ctx.krate_spans;
    let (mut tree_skipper, build_diags) = TreeSkipper::build(tree_skipped, ctx.krates, file_id);

    if !build_diags.is_empty() {
        sink.push(build_diags);
    }

    let (denied_ids, ban_wrappers): (Vec<_>, Vec<_>) =
        denied.into_iter().map(|kb| (kb.id, kb.wrappers)).unzip();

    // Keep track of all the crates we skip, and emit a warning if
    // we encounter a skip that didn't actually match any crate version
    // so that people can clean up their config files
    let mut skip_hit: BitVec = BitVec::repeat(false, skipped.len());

    struct MultiDetector<'a> {
        name: &'a str,
        dupes: smallvec::SmallVec<[usize; 2]>,
    }

    let mut multi_detector = MultiDetector {
        name: &ctx.krates.krates().next().unwrap().krate.name,
        dupes: smallvec::SmallVec::new(),
    };

    let report_duplicates = |multi_detector: &MultiDetector<'_>, sink: &mut diag::ErrorSink| {
        if multi_detector.dupes.len() <= 1 || multiple_versions == LintLevel::Allow {
            return;
        }

        let severity = match multiple_versions {
            LintLevel::Warn => Severity::Warning,
            LintLevel::Deny => Severity::Error,
            LintLevel::Allow => unreachable!(),
        };

        let mut all_start = std::usize::MAX;
        let mut all_end = 0;

        struct Dupe {
            /// Unique id, used for printing the actual diagnostic graphs
            id: Kid,
            /// Version, for deterministically ordering the duplicates
            version: semver::Version,
        }

        let mut kids = smallvec::SmallVec::<[Dupe; 2]>::new();

        for dup in multi_detector.dupes.iter().cloned() {
            let span = &ctx.krate_spans[dup];

            if span.start < all_start {
                all_start = span.start;
            }

            if span.end > all_end {
                all_end = span.end;
            }

            let krate = &ctx.krates[dup];

            if let Err(i) = kids.binary_search_by(|other| match other.version.cmp(&krate.version) {
                std::cmp::Ordering::Equal => other.id.cmp(&krate.id),
                ord => ord,
            }) {
                kids.insert(
                    i,
                    Dupe {
                        id: krate.id.clone(),
                        version: krate.version.clone(),
                    },
                );
            }
        }

        {
            let mut diag: Diag = diags::Duplicates {
                krate_name: multi_detector.name,
                num_dupes: kids.len(),
                krates_coord: KrateCoord {
                    file: krate_spans.file_id,
                    span: all_start..all_end,
                },
                severity,
            }
            .into();

            diag.kids = kids.into_iter().map(|dupe| dupe.id).collect();

            let mut pack = Pack::new(Check::Bans);
            pack.push(diag);

            sink.push(pack);
        }

        if let Some(ref og) = output_graph {
            match graph::create_graph(
                multi_detector.name,
                highlight,
                ctx.krates,
                &multi_detector.dupes,
            ) {
                Ok(graph) => {
                    if let Err(e) = og(DupGraph {
                        duplicate: multi_detector.name.to_owned(),
                        graph,
                    }) {
                        log::error!("{}", e);
                    }
                }
                Err(e) => {
                    log::error!("unable to create graph for {}: {}", multi_detector.name, e);
                }
            };
        }
    };

    for (i, krate) in ctx.krates.krates().map(|kn| &kn.krate).enumerate() {
        let mut pack = Pack::with_kid(Check::Bans, krate.id.clone());

        if let Some(matches) = matches(&denied_ids, krate) {
            for rm in matches {
                let ban_cfg = CfgCoord {
                    file: file_id,
                    span: rm.id.span.clone(),
                };

                // The crate is banned, but it might have be allowed if it's wrapped
                // by one or more particular crates
                let wrappers = ban_wrappers.get(rm.index);
                let is_allowed = match wrappers {
                    Some(wrappers) => {
                        let nid = ctx.krates.nid_for_kid(&krate.id).unwrap();
                        let graph = ctx.krates.graph();

                        // Ensure that every single crate that has a direct dependency
                        // on the banned crate is an allowed wrapper
                        graph
                            .edges_directed(nid, Direction::Incoming)
                            .map(|edge| edge.source())
                            .all(|nid| {
                                let node = &graph[nid];

                                let (diag, is_allowed): (Diag, _) =
                                    match wrappers.iter().find(|aw| aw.value == node.krate.name) {
                                        Some(aw) => (
                                            diags::BannedAllowedByWrapper {
                                                ban_cfg: ban_cfg.clone(),
                                                ban_exception_cfg: CfgCoord {
                                                    file: file_id,
                                                    span: aw.span.clone(),
                                                },
                                                banned_krate: krate,
                                                wrapper_krate: &node.krate,
                                            }
                                            .into(),
                                            true,
                                        ),
                                        None => (
                                            diags::BannedUnmatchedWrapper {
                                                ban_cfg: ban_cfg.clone(),
                                                banned_krate: krate,
                                                parent_krate: &node.krate,
                                            }
                                            .into(),
                                            false,
                                        ),
                                    };

                                pack.push(diag);
                                is_allowed
                            })
                    }
                    None => false,
                };

                if !is_allowed {
                    pack.push(diags::ExplicitlyBanned { krate, ban_cfg });
                }
            }
        }

        if !allowed.is_empty() {
            // Since only allowing specific crates is pretty draconian,
            // also emit which allow filters actually passed each crate
            match matches(&allowed, krate) {
                Some(matches) => {
                    for rm in matches {
                        pack.push(diags::ExplicitlyAllowed {
                            krate,
                            allow_cfg: CfgCoord {
                                file: file_id,
                                span: rm.id.span.clone(),
                            },
                        });
                    }
                }
                None => {
                    pack.push(diags::ImplicitlyBanned { krate });
                }
            }
        }

        if let Some(matches) = matches(&skipped, krate) {
            for rm in matches {
                pack.push(diags::Skipped {
                    krate,
                    skip_cfg: CfgCoord {
                        file: file_id,
                        span: rm.id.span.clone(),
                    },
                });

                // Keep a count of the number of times each skip filter is hit
                // so that we can report unused filters to the user so that they
                // can cleanup their configs as their dependency graph changes over time
                skip_hit.as_mut_bitslice().set(rm.index, true);
            }
        } else if !tree_skipper.matches(krate, &mut pack) {
            if multi_detector.name != krate.name {
                report_duplicates(&multi_detector, &mut sink);

                multi_detector.name = &krate.name;
                multi_detector.dupes.clear();
            }

            multi_detector.dupes.push(i);

            if wildcards != LintLevel::Allow {
                let severity = match wildcards {
                    LintLevel::Warn => Severity::Warning,
                    LintLevel::Deny => Severity::Error,
                    LintLevel::Allow => unreachable!(),
                };

                let wildcards: Vec<_> = krate
                    .deps
                    .iter()
                    .filter(|dep| dep.req == wildcard)
                    .collect();

                if !wildcards.is_empty() {
                    sink.push(diags::Wildcards {
                        krate,
                        severity,
                        wildcards,
                        cargo_spans: &cargo_spans,
                    });
                }
            }
        }

        if let Some(allow_build_scripts) = &allow_build_scripts {
            let has_build_script = krate
                .targets
                .iter()
                .any(|t| t.kind.iter().any(|k| *k == "custom-build"));

            if has_build_script {
                let allowed_build_script = allow_build_scripts.value.iter().any(|id| {
                    krate.name == id.name && crate::match_req(&krate.version, id.version.as_ref())
                });

                if !allowed_build_script {
                    pack.push(diags::BuildScriptNotAllowed { krate });
                }
            }
        }

        if !pack.is_empty() {
            sink.push(pack);
        }
    }

    report_duplicates(&multi_detector, &mut sink);

    let mut pack = Pack::new(Check::Bans);

    for skip in skip_hit
        .into_iter()
        .zip(skipped.into_iter())
        .filter_map(|(hit, skip)| if !hit { Some(skip) } else { None })
    {
        pack.push(diags::UnmatchedSkip {
            skip_cfg: CfgCoord {
                file: file_id,
                span: skip.span,
            },
        });
    }

    sink.push(pack);
}
