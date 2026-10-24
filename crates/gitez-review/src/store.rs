//! Where local pull requests are kept, and how they are written safely.
//!
//! ```text
//! <git common dir>/gitez/reviews/
//!   3/review.json                       the pull request
//!   3/review.json.lock                  only while it is being saved
//!   3/runs/2026-10-24T13-02-11Z.jsonl   one agent run
//! ```

use std::fs::{self, File, OpenOptions};
use std::io::{BufRead as _, BufReader, ErrorKind, Write as _};
use std::path::{Path, PathBuf};

use jiff::Timestamp;
use serde::{Deserialize, Serialize};

use crate::model::{PromptOptions, PullRequest, State, Tip};
use crate::{Error, Result, VERSION};

const REVIEW_FILE: &str = "review.json";
const LOCK_FILE: &str = "review.json.lock";
const RUNS_DIR: &str = "runs";

/// The local pull requests of one repository.
#[derive(Debug, Clone)]
pub struct Store {
    dir: PathBuf,
}

/// What a new local pull request starts with.
#[derive(Debug, Clone)]
pub struct NewPullRequest {
    pub title: String,
    pub description: String,
    pub base: Tip,
    pub head: Tip,
}

/// The first line of a transcript: what was sent to which agent, and when.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunHeader {
    pub started: Timestamp,
    pub agent: String,
    pub prompt: String,
}

/// The transcript of an agent run being written.
#[derive(Debug)]
pub struct Run {
    path: PathBuf,
    file: File,
}

/// `review.json` as written: the pull request with the format version.
#[derive(Serialize)]
struct OnDisk<'a> {
    version: u32,
    #[serde(flatten)]
    pull_request: &'a PullRequest,
}

/// Only the version, read before the rest so a newer format is refused
/// rather than misread.
#[derive(Deserialize)]
struct VersionOnly {
    version: u32,
}

/// A transcript's header line, marked so it is not taken for agent output.
#[derive(Serialize, Deserialize)]
struct HeaderLine {
    gitez: String,
    version: u32,
    #[serde(flatten)]
    header: RunHeader,
}

impl Store {
    /// The store of the repository whose common git directory (the main
    /// `.git`, shared by its worktrees) is `common_dir`.
    pub fn new(common_dir: &Path) -> Self {
        Self {
            dir: common_dir.join("gitez").join("reviews"),
        }
    }

    /// The directory holding one folder per pull request.
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// The numbers of the pull requests, in order. A missing store has
    /// none.
    pub fn numbers(&self) -> Result<Vec<u32>> {
        let entries = match fs::read_dir(&self.dir) {
            Ok(entries) => entries,
            Err(error) if error.kind() == ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => return Err(Error::io(&self.dir)(error)),
        };
        let mut numbers = Vec::new();
        for entry in entries {
            let entry = entry.map_err(Error::io(&self.dir))?;
            let Some(number) = entry
                .file_name()
                .to_str()
                .and_then(|name| name.parse::<u32>().ok())
            else {
                continue;
            };
            if entry.path().join(REVIEW_FILE).is_file() {
                numbers.push(number);
            }
        }
        numbers.sort_unstable();
        Ok(numbers)
    }

    /// Reads pull request `number`.
    pub fn load(&self, number: u32) -> Result<PullRequest> {
        let path = self.review_path(number);
        let text = match fs::read_to_string(&path) {
            Ok(text) => text,
            Err(error) if error.kind() == ErrorKind::NotFound => {
                return Err(Error::NotFound(number));
            }
            Err(error) => return Err(Error::io(path)(error)),
        };
        parse(&path, &text)
    }

    /// Creates a pull request, numbered one above the highest so far, and
    /// saves it.
    pub fn create(&self, new: NewPullRequest, at: Timestamp) -> Result<PullRequest> {
        fs::create_dir_all(&self.dir).map_err(Error::io(&self.dir))?;
        let mut number = self.numbers()?.last().map_or(1, |last| last + 1);
        // Creating the folder claims the number, even against another
        // GitEZ creating one at the same time.
        loop {
            let dir = self.dir.join(number.to_string());
            match fs::create_dir(&dir) {
                Ok(()) => break,
                Err(error) if error.kind() == ErrorKind::AlreadyExists => number += 1,
                Err(error) => return Err(Error::io(dir)(error)),
            }
        }
        let pull_request = PullRequest {
            number,
            title: new.title,
            description: new.description,
            state: State::Open,
            base: new.base,
            head: new.head,
            created: at,
            updated: at,
            viewed: Default::default(),
            prompt: PromptOptions::default(),
            threads: Vec::new(),
        };
        self.save(&pull_request)?;
        Ok(pull_request)
    }

    /// Writes `pull_request` over its saved version. The new content goes
    /// to a lock file first, which is then renamed over `review.json`, so
    /// the file is never half written and two writers cannot interleave.
    pub fn save(&self, pull_request: &PullRequest) -> Result<()> {
        let dir = self.dir.join(pull_request.number.to_string());
        fs::create_dir_all(&dir).map_err(Error::io(&dir))?;
        let path = dir.join(REVIEW_FILE);
        let lock_path = dir.join(LOCK_FILE);
        let lock = Lock::take(&lock_path)?;

        // Never overwrite what a newer GitEZ wrote.
        match fs::read_to_string(&path) {
            Ok(text) => {
                if let Ok(VersionOnly { version }) = serde_json::from_str(&text)
                    && version > VERSION
                {
                    return Err(Error::TooNew { path, version });
                }
            }
            Err(error) if error.kind() == ErrorKind::NotFound => {}
            Err(error) => return Err(Error::io(path)(error)),
        }

        let mut text = serde_json::to_string_pretty(&OnDisk {
            version: VERSION,
            pull_request,
        })
        .map_err(|source| Error::Invalid {
            path: path.clone(),
            source,
        })?;
        text.push('\n');
        let mut file = &lock.file;
        file.write_all(text.as_bytes())
            .and_then(|()| file.sync_all())
            .map_err(Error::io(&lock_path))?;
        lock.commit(&path)
    }

    /// Starts the transcript of an agent run for pull request `number`,
    /// writing `header` as its first line.
    pub fn start_run(&self, number: u32, header: RunHeader) -> Result<Run> {
        let dir = self.dir.join(number.to_string()).join(RUNS_DIR);
        fs::create_dir_all(&dir).map_err(Error::io(&dir))?;
        // A UTC time without colons, which some file systems refuse.
        let stem = header.started.strftime("%Y-%m-%dT%H-%M-%SZ").to_string();
        let mut attempt = 1;
        let (path, file) = loop {
            let name = match attempt {
                1 => format!("{stem}.jsonl"),
                n => format!("{stem}-{n}.jsonl"),
            };
            let path = dir.join(name);
            match OpenOptions::new().append(true).create_new(true).open(&path) {
                Ok(file) => break (path, file),
                Err(error) if error.kind() == ErrorKind::AlreadyExists => attempt += 1,
                Err(error) => return Err(Error::io(path)(error)),
            }
        };
        let line = serde_json::to_string(&HeaderLine {
            gitez: "run".to_owned(),
            version: VERSION,
            header,
        })
        .map_err(|source| Error::Invalid {
            path: path.clone(),
            source,
        })?;
        let mut run = Run { path, file };
        run.append(&line)?;
        Ok(run)
    }

    /// The transcripts of pull request `number`'s agent runs, oldest first.
    pub fn runs(&self, number: u32) -> Result<Vec<PathBuf>> {
        let dir = self.dir.join(number.to_string()).join(RUNS_DIR);
        let entries = match fs::read_dir(&dir) {
            Ok(entries) => entries,
            Err(error) if error.kind() == ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => return Err(Error::io(dir)(error)),
        };
        let mut runs = Vec::new();
        for entry in entries {
            let path = entry.map_err(Error::io(&dir))?.path();
            if path
                .extension()
                .is_some_and(|extension| extension == "jsonl")
            {
                runs.push(path);
            }
        }
        runs.sort_by_key(|path| run_order(path));
        Ok(runs)
    }

    /// Reads the transcript at `path`: its header and the agent's output
    /// lines.
    pub fn read_run(&self, path: &Path) -> Result<(RunHeader, Vec<String>)> {
        let file = File::open(path).map_err(Error::io(path))?;
        let mut lines = BufReader::new(file).lines();
        let first = lines
            .next()
            .transpose()
            .map_err(Error::io(path))?
            .unwrap_or_default();
        let header: HeaderLine = serde_json::from_str(&first).map_err(|source| Error::Invalid {
            path: path.to_owned(),
            source,
        })?;
        if header.version > VERSION {
            return Err(Error::TooNew {
                path: path.to_owned(),
                version: header.version,
            });
        }
        let output = lines.collect::<Result<_, _>>().map_err(Error::io(path))?;
        Ok((header.header, output))
    }

    fn review_path(&self, number: u32) -> PathBuf {
        self.dir.join(number.to_string()).join(REVIEW_FILE)
    }
}

impl Run {
    /// Where the transcript is written.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Adds one line of the agent's output, such as a stream-JSON event.
    /// The line is written at once, so a crash loses nothing before it.
    pub fn append(&mut self, line: &str) -> Result<()> {
        let mut text = line.trim_end_matches(['\r', '\n']).to_owned();
        text.push('\n');
        self.file
            .write_all(text.as_bytes())
            .map_err(Error::io(&self.path))
    }
}

/// Where the transcript at `path` sorts: by the time in its name, then by
/// the attempt that made the name unique (`<time>-2.jsonl`).
fn run_order(path: &Path) -> (String, u32) {
    let stem = path
        .file_stem()
        .map(|stem| stem.to_string_lossy().into_owned())
        .unwrap_or_default();
    match stem.rsplit_once('-') {
        Some((time, attempt)) if time.ends_with('Z') => {
            (time.to_owned(), attempt.parse().unwrap_or(u32::MAX))
        }
        _ => (stem, 1),
    }
}

/// `review.json`'s content at `path`, if this GitEZ understands its format.
fn parse(path: &Path, text: &str) -> Result<PullRequest> {
    let invalid = |source| Error::Invalid {
        path: path.to_owned(),
        source,
    };
    let VersionOnly { version } = serde_json::from_str(text).map_err(invalid)?;
    if version > VERSION {
        return Err(Error::TooNew {
            path: path.to_owned(),
            version,
        });
    }
    serde_json::from_str(text).map_err(invalid)
}

/// An exclusively created lock file, removed on drop unless renamed over
/// its target.
struct Lock {
    path: PathBuf,
    file: File,
    committed: bool,
}

impl Lock {
    fn take(path: &Path) -> Result<Self> {
        match OpenOptions::new().write(true).create_new(true).open(path) {
            Ok(file) => Ok(Self {
                path: path.to_owned(),
                file,
                committed: false,
            }),
            Err(error) if error.kind() == ErrorKind::AlreadyExists => Err(Error::Locked {
                path: path.to_owned(),
            }),
            Err(error) => Err(Error::io(path)(error)),
        }
    }

    /// Renames the lock file, with what was written to it, to `target`.
    fn commit(mut self, target: &Path) -> Result<()> {
        fs::rename(&self.path, target).map_err(Error::io(target))?;
        self.committed = true;
        Ok(())
    }
}

impl Drop for Lock {
    fn drop(&mut self) {
        if !self.committed {
            // Best effort: a lock left behind is reported on the next save.
            let _ = fs::remove_file(&self.path);
        }
    }
}
