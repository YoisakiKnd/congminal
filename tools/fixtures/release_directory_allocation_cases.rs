#[cfg(test)]
mod release_directory_probe {
    use super::*;
    use crate::{executor_allocation_meter as meter, resources::{Budget, Scope}};
    use std::sync::Arc;
    struct Tree { fanout: bool, calls: usize }
    impl Lister for Tree {
        fn list(&mut self, path: &RemotePath) -> Result<Page, Error> {
            self.calls += 1;
            let mut scan = super::super::model::Scan::new(1, 1, path.clone(), true, Sort::Name)?;
            let directory = self.fanout || self.calls == 1;
            let count = if directory { 4 } else { FILES / 4 };
            let attrs = super::super::model::Attributes {
                permissions: Some(if directory { 0o040700 } else { 0o100600 }),
                size: Some(1), times: Some((1, 1)), ..Default::default()
            };
            for index in 0..count {
                let name = format!("{index:04}-bounded-directory-raw-name");
                if scan.push_entry(name.as_bytes(), attrs, &Cancellation::default())? { break; }
            }
            let listing = scan.finish(true)?;
            Ok(Page { incomplete: listing.incomplete, entries: listing.into_entries() })
        }
    }
    #[test]
    fn retained_directory_plan_and_scan_transients_are_funded() {
        for fanout in [false, true] {
            let pool = Budget::new(16 * 1024 * 1024, 0);
            let _scope = Scope::enter(pool.clone());
            // Initialize shared accounting contexts before measurement; no page/table/path is prebuilt.
            let _ = pool.snapshot();
            meter::start();
            let root = RemotePath::new(&pool, b"/root").unwrap();
            let mut tree = Tree { fanout, calls: 0 };
            let plan = walk(&mut tree, &root, &Cancellation::default()).unwrap();
            assert!(plan.items.len() <= FILES);
            assert!(plan.directories <= DIRECTORIES && tree.calls <= DIRECTORIES);
            assert_eq!(plan.directories, plan.directory_paths.len());
            if fanout { assert!(plan.truncated && plan.directories > 1); }
            else { assert_eq!(plan.items.len(), FILES); }
            let measured = meter::peek();
            let admitted = pool.snapshot();
            assert!(measured.live > 0 && measured.live as usize <= admitted.used);
            assert!(measured.peak as usize <= admitted.peak);
            let files = plan.items.len();
            let directories = plan.directories;
            drop((plan, root));
            let final_counts = meter::stop();
            assert_eq!(final_counts.live, 0);
            assert_eq!(pool.snapshot().used, 0);
            eprintln!("DIRECTORY_ALLOC mode={} files={files} directories={directories} live={} peak={} used={} admitted_peak={} final=0",
                if fanout { "fanout" } else { "files" }, measured.live, measured.peak, admitted.used, admitted.peak);
            drop(Arc::clone(&pool));
        }
    }
}
