//! Times reading refs, walking the whole history and laying out the graph.
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
    Ok(())
}
