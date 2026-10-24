//! Times reading refs, walking the whole history, laying out the graph and
//! reading the details (message and changed files) and diffs of the newest
//! commits, and the working-tree status.
//!
//! ```sh
//! cargo run --release -p gitez-git --example history -- <repository>
//! ```

use std::time::Instant;

use gitez_git::graph::GraphLayout;
use gitez_git::{Order, Repository};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let path = std::env::args().nth(1).unwrap_or_else(|| ".".to_owned());

    let start = Instant::now();
    let repo = Repository::open(&path)?;
    let refs = repo.refs()?;
    println!(
        "refs: {} branches, {} remote branches, {} tags, {} stashes in {:?}",
        refs.branches.len(),
        refs.remote_branches.len(),
        refs.tags.len(),
        refs.stashes.len(),
        start.elapsed()
    );

    let start = Instant::now();
    let mut walk = repo.walk(refs.tips(true), Order::Date)?;
    let first = walk.next_batch(200)?;
    let first_batch = start.elapsed();

    let newest: Vec<_> = first.iter().map(|commit| commit.id).collect();
    let mut layout = GraphLayout::new();
    let mut commits = 0;
    let mut max_width = 0;
    let mut batch = first;
    while !batch.is_empty() {
        for commit in &batch {
            max_width = max_width.max(layout.push(commit.id, &commit.parents).width);
        }
        commits += batch.len();
        batch = walk.next_batch(1000)?;
    }
    println!(
        "history: {commits} commits, first 200 in {first_batch:?}, all in {:?}, widest row {max_width} lanes",
        start.elapsed()
    );

    let mut slowest = (Default::default(), newest.first().copied());
    let mut files = 0;
    let start = Instant::now();
    for &id in &newest {
        let started = Instant::now();
        files += repo.commit_details(id)?.files.len();
        let elapsed = started.elapsed();
        if elapsed > slowest.0 {
            slowest = (elapsed, Some(id));
        }
    }
    if let (elapsed, Some(id)) = slowest {
        println!(
            "details: {} commits, {files} changed files in {:?}, slowest {id} in {elapsed:?}",
            newest.len(),
            start.elapsed()
        );
    }

    let mut slowest = (Default::default(), newest.first().copied());
    let mut lines = 0;
    let start = Instant::now();
    for &id in &newest {
        let started = Instant::now();
        for file in repo.commit_diff(id, Default::default())? {
            if let gitez_git::DiffContent::Text(hunks) = file.content {
                lines += hunks.iter().map(|hunk| hunk.lines.len()).sum::<usize>();
            }
        }
        let elapsed = started.elapsed();
        if elapsed > slowest.0 {
            slowest = (elapsed, Some(id));
        }
    }
    if let (elapsed, Some(id)) = slowest {
        println!(
            "diffs: {} commits, {lines} diff lines in {:?}, slowest {id} in {elapsed:?}",
            newest.len(),
            start.elapsed()
        );
    }

    for run in ["first", "second"] {
        let start = Instant::now();
        let status = repo.status()?;
        println!(
            "status ({run} read): {} files, {} staged in {:?}",
            status.files.len(),
            status.staged(),
            start.elapsed()
        );
    }
    Ok(())
}
