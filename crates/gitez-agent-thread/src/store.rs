//! Where a repository's agent threads are kept, and how they are written
//! safely.
//!
//! ```text
//! <git common dir>/gitez/threads/
//!   add-a-blame-view/thread.json                       the agent thread
//!   add-a-blame-view/thread.json.lock                  only while it is saved
//!   add-a-blame-view/runs/2026-11-13T09-41-31Z.jsonl   one turn's transcript
//!   add-a-blame-view/images/2026-11-13T09-41-31Z-1.png an image sent with a turn
//! ```

use std::borrow::Borrow;
use std::fs::{self, File, OpenOptions};
use std::io::{BufRead as _, BufReader, ErrorKind, Read as _, Seek as _, SeekFrom, Write as _};
use std::path::{Path, PathBuf};

use gitez_sandbox::NetworkProfile;
use jiff::Timestamp;
use serde::{Deserialize, Serialize};

use crate::lock::Lock;
use crate::model::{AgentThread, AgentThreadId, NewAgentThread, State, Turn, TurnKind};
use crate::{
    Error, MAX_INPUT_BYTES, PreparedImage, Result, TurnImage, VERSION, slug, title_from_request,
    worktree_path,
};

const THREAD_FILE: &str = "thread.json";
const LOCK_FILE: &str = "thread.json.lock";
const RUNS_DIR: &str = "runs";
const IMAGES_DIR: &str = "images";
/// The longest image file name GitEZ makes and reads.
const MAX_IMAGE_NAME: usize = 64;
/// How many ids `create` tries (`<slug>`, `<slug>-2`, …) before giving up.
const MAX_ID_ATTEMPTS: u32 = 1000;

/// The agent threads of one repository: `<common dir>/gitez/threads/`.
#[derive(Debug, Clone)]
pub struct Store {
    dir: PathBuf,
}

/// The first line of a transcript: what was sent to which agent, and how.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunHeader {
    pub started: Timestamp,
    pub agent: String,
    pub prompt: String,
    pub kind: TurnKind,
    pub session_id: Option<String>,
    pub resume: bool,
    pub model: Option<String>,
    pub network: NetworkProfile,
    /// The names of the images sent with the prompt (never their data).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub images: Vec<String>,
}

/// A transcript being written.
#[derive(Debug)]
pub struct Run {
    path: PathBuf,
    file: File,
}

/// `thread.json` as written: the agent thread with the format version.
#[derive(Serialize)]
struct OnDisk<'a> {
    version: u32,
    #[serde(flatten)]
    thread: &'a AgentThread,
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
            dir: common_dir.join("gitez").join("threads"),
        }
    }

    /// The directory holding one folder per agent thread.
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// The folder of agent thread `id`.
    pub fn thread_dir(&self, id: &AgentThreadId) -> PathBuf {
        self.dir.join(id.as_str())
    }

    /// Folders with a thread.json, sorted. A missing store has none.
    pub fn ids(&self) -> Result<Vec<AgentThreadId>> {
        let entries = match fs::read_dir(&self.dir) {
            Ok(entries) => entries,
            Err(error) if error.kind() == ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => return Err(Error::io(&self.dir)(error)),
        };
        let mut ids = Vec::new();
        for entry in entries {
            let entry = entry.map_err(Error::io(&self.dir))?;
            let Some(id) = entry
                .file_name()
                .to_str()
                .and_then(|name| AgentThreadId::new(name).ok())
            else {
                continue;
            };
            if entry.path().join(THREAD_FILE).is_file() {
                ids.push(id);
            }
        }
        ids.sort_unstable();
        Ok(ids)
    }

    /// Reads agent thread `id`.
    pub fn load(&self, id: &AgentThreadId) -> Result<AgentThread> {
        let path = self.thread_path(id);
        let text = match fs::read_to_string(&path) {
            Ok(text) => text,
            Err(error) if error.kind() == ErrorKind::NotFound => {
                return Err(Error::NotFound(id.clone()));
            }
            Err(error) => return Err(Error::io(path)(error)),
        };
        parse(&path, &text)
    }

    /// Every readable agent thread, newest first, and the errors of the
    /// others. One unreadable thread does not hide the rest. A thread being
    /// created (its folder exists, its file not yet) is left out. When the
    /// store itself cannot be listed, both are empty; call
    /// [`ids`](Self::ids) to learn why.
    pub fn load_all(&self) -> (Vec<AgentThread>, Vec<(AgentThreadId, Error)>) {
        let Ok(ids) = self.ids() else {
            return (Vec::new(), Vec::new());
        };
        let mut threads = Vec::with_capacity(ids.len());
        let mut errors = Vec::new();
        for id in ids {
            match self.load(&id) {
                Ok(thread) => threads.push(thread),
                Err(Error::NotFound(_)) => {}
                Err(error) => errors.push((id, error)),
            }
        }
        threads.sort_by(|a, b| b.created.cmp(&a.created).then_with(|| a.id.cmp(&b.id)));
        (threads, errors)
    }

    /// Creates an agent thread: title from the prompt, id from the title,
    /// made unique by claiming its folder with `create_dir` and skipping
    /// ids for which `taken(branch)` is true or whose worktree folder
    /// exists (`-2`, `-3`, …); worktree at
    /// `worktree_path(data_dir, repository_name, common_dir, id)`; state
    /// Queued; one Start turn with the prompt, resume false. Saves it.
    pub fn create(
        &self,
        new: NewAgentThread,
        data_dir: &Path,
        repository_name: &str,
        taken: impl Fn(&str) -> bool,
        at: Timestamp,
    ) -> Result<AgentThread> {
        let images: &[PreparedImage] = &[];
        self.create_with_images(new, images, data_dir, repository_name, taken, at)
    }

    /// [`create`](Self::create), with `images` sent with the first prompt:
    /// written to the thread's `images/` before its `thread.json`, and
    /// listed in its Start turn. A prompt with no words but images gets the
    /// title of [`title_from_request`].
    pub fn create_with_images(
        &self,
        new: NewAgentThread,
        images: &[impl Borrow<PreparedImage>],
        data_dir: &Path,
        repository_name: &str,
        taken: impl Fn(&str) -> bool,
        at: Timestamp,
    ) -> Result<AgentThread> {
        fs::create_dir_all(&self.dir).map_err(Error::io(&self.dir))?;
        let common_dir = self.common_dir();
        let title = title_from_request(&new.prompt, images.len());
        let base = slug(&title);
        let mut attempt = 1u32;
        let (id, worktree) = loop {
            if attempt > MAX_ID_ATTEMPTS {
                return Err(Error::refused(format!(
                    "Every name for this thread is taken, up to {base}-{MAX_ID_ATTEMPTS}. \
                     Complete some threads named like it, or start it with another prompt."
                )));
            }
            let candidate = match attempt {
                1 => base.clone(),
                n => format!("{base}-{n}"),
            };
            attempt += 1;
            let id = AgentThreadId::new(candidate)?;
            let worktree = worktree_path(data_dir, repository_name, &common_dir, &id);
            if taken(&id.branch()) || worktree.exists() {
                continue;
            }
            // Creating the folder claims the id, even against another GitEZ
            // creating a thread at the same time.
            let dir = self.thread_dir(&id);
            match fs::create_dir(&dir) {
                Ok(()) => break (id, worktree),
                Err(error) if error.kind() == ErrorKind::AlreadyExists => {}
                Err(error) => return Err(Error::io(dir)(error)),
            }
        };
        let turn_images = match self.write_images(&id, images, at) {
            Ok(written) => written,
            Err(error) => {
                // The folder claimed the id; with no thread.json it is not a
                // thread, and the id is free again.
                fs::remove_dir_all(self.thread_dir(&id)).ok();
                return Err(error);
            }
        };
        let mut first = Turn::first(new.prompt, at);
        first.images = turn_images;
        let thread = AgentThread {
            branch: id.branch(),
            id,
            title,
            agent: new.agent,
            model: new.model,
            state: State::Queued,
            ending: None,
            created: at,
            updated: at,
            start: new.start,
            worktree,
            prepared: false,
            network: new.network,
            secrets: new.secrets,
            session_id: None,
            pull_request: None,
            turns: vec![first],
            activity: None,
            waiting_until: None,
            error: None,
            warnings: Vec::new(),
            supervisor: None,
            completed: None,
            worktree_record: None,
            uncommitted: None,
            deleted_branch: None,
        };
        let lock = Lock::take(&self.thread_dir(&thread.id).join(LOCK_FILE))?;
        self.write(lock, &thread)?;
        Ok(thread)
    }

    /// Reads the agent thread, applies `change`, and writes it back if it
    /// changed, all while holding `thread.json.lock` (created exclusively;
    /// when it exists, retried every 20 ms for up to 5 s, then
    /// [`Error::Locked`]). `change` returning Err writes nothing. Sets
    /// `updated` to now when it writes. Refuses ([`Error::TooNew`]) a newer
    /// version.
    pub fn update<T>(
        &self,
        id: &AgentThreadId,
        change: impl FnOnce(&mut AgentThread) -> Result<T>,
    ) -> Result<T> {
        let dir = self.thread_dir(id);
        if !dir.join(THREAD_FILE).is_file() {
            return Err(Error::NotFound(id.clone()));
        }
        let lock = Lock::take(&dir.join(LOCK_FILE))?;
        let original = self.load(id)?;
        let mut thread = original.clone();
        let value = change(&mut thread)?;
        if thread != original {
            thread.updated = Timestamp::now();
            self.write(lock, &thread)?;
        }
        Ok(value)
    }

    /// Writes `thread` through `lock`, which the caller holds.
    fn write(&self, lock: Lock, thread: &AgentThread) -> Result<()> {
        let path = self.thread_path(&thread.id);
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
            thread,
        })
        .map_err(|source| Error::Invalid {
            path: path.clone(),
            source,
        })?;
        text.push('\n');
        lock.write_and_commit(&text, &path)
    }

    /// Writes `images` to the thread's `images/` folder, as files named
    /// `<UTC time>-<n>.<extension>` made exclusively, and returns them as
    /// a turn lists them, in order. On an error, what was written is
    /// removed. The thread must exist.
    pub fn add_images(
        &self,
        id: &AgentThreadId,
        images: &[impl Borrow<PreparedImage>],
        at: Timestamp,
    ) -> Result<Vec<TurnImage>> {
        if !self.thread_dir(id).join(THREAD_FILE).is_file() {
            return Err(Error::NotFound(id.clone()));
        }
        self.write_images(id, images, at)
    }

    fn write_images(
        &self,
        id: &AgentThreadId,
        images: &[impl Borrow<PreparedImage>],
        at: Timestamp,
    ) -> Result<Vec<TurnImage>> {
        if images.is_empty() {
            return Ok(Vec::new());
        }
        let dir = self.thread_dir(id).join(IMAGES_DIR);
        fs::create_dir_all(&dir).map_err(Error::io(&dir))?;
        let stem = at.strftime("%Y-%m-%dT%H-%M-%SZ").to_string();
        let mut written: Vec<TurnImage> = Vec::with_capacity(images.len());
        let mut number = 1_u32;
        for image in images {
            let image: &PreparedImage = image.borrow();
            let result = loop {
                let name = format!("{stem}-{number}.{}", image.media_type.extension());
                number += 1;
                let path = dir.join(&name);
                match OpenOptions::new().write(true).create_new(true).open(&path) {
                    Ok(mut file) => {
                        break file
                            .write_all(&image.bytes)
                            .and_then(|()| file.sync_all())
                            .map(|()| TurnImage {
                                name,
                                media_type: image.media_type,
                                size: image.bytes.len() as u64,
                                width: image.width,
                                height: image.height,
                            })
                            .map_err(|error| {
                                fs::remove_file(&path).ok();
                                Error::io(path)(error)
                            });
                    }
                    Err(error) if error.kind() == ErrorKind::AlreadyExists => {}
                    Err(error) => break Err(Error::io(path)(error)),
                }
            };
            match result {
                Ok(image) => written.push(image),
                Err(error) => {
                    self.remove_images(id, &written);
                    return Err(error);
                }
            }
        }
        Ok(written)
    }

    /// Removes the files of `images`, as after a turn that could not be
    /// asked for; best effort.
    pub fn remove_images(&self, id: &AgentThreadId, images: &[TurnImage]) {
        for image in images {
            if let Ok(path) = self.image_path(id, image) {
                fs::remove_file(path).ok();
            }
        }
    }

    /// Where the file of `image` is: `images/<name>` in the thread's
    /// folder. Refuses ([`Error::Refused`]) a name GitEZ would not make,
    /// so a changed `thread.json` cannot point outside the folder.
    pub fn image_path(&self, id: &AgentThreadId, image: &TurnImage) -> Result<PathBuf> {
        let name = &image.name;
        let valid = !name.is_empty()
            && name.len() <= MAX_IMAGE_NAME
            && !name.starts_with('.')
            && name
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_'));
        if !valid {
            return Err(Error::refused(format!(
                "{name} is not an image GitEZ made."
            )));
        }
        Ok(self.thread_dir(id).join(IMAGES_DIR).join(name))
    }

    /// The bytes of `image`. Refuses a file that is a symbolic link, is
    /// larger than any image GitEZ writes, or is not the size the turn
    /// lists.
    pub fn read_image(&self, id: &AgentThreadId, image: &TurnImage) -> Result<Vec<u8>> {
        let path = self.image_path(id, image)?;
        let metadata = fs::symlink_metadata(&path).map_err(Error::io(&path))?;
        if !metadata.is_file() || metadata.len() > MAX_INPUT_BYTES || metadata.len() != image.size {
            return Err(Error::refused(format!(
                "The image {} is not as GitEZ saved it.",
                image.name
            )));
        }
        fs::read(&path).map_err(Error::io(path))
    }

    /// Starts `runs/<UTC time>.jsonl` with a header line. Returns the run;
    /// its file name goes into the turn's `transcript`.
    pub fn start_run(&self, id: &AgentThreadId, header: RunHeader) -> Result<Run> {
        let thread_dir = self.thread_dir(id);
        if !thread_dir.join(THREAD_FILE).is_file() {
            return Err(Error::NotFound(id.clone()));
        }
        let dir = thread_dir.join(RUNS_DIR);
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

    /// Transcript paths, oldest first.
    pub fn runs(&self, id: &AgentThreadId) -> Result<Vec<PathBuf>> {
        let dir = self.thread_dir(id).join(RUNS_DIR);
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
        let header = parse_header(path, &first)?;
        let output = lines.collect::<Result<_, _>>().map_err(Error::io(path))?;
        Ok((header, output))
    }

    /// Lines after byte `offset` (only whole lines), and the new offset,
    /// for following a transcript while it is written. Offset 0 skips the
    /// header line.
    pub fn read_run_from(&self, path: &Path, offset: u64) -> Result<(Vec<String>, u64)> {
        let mut file = File::open(path).map_err(Error::io(path))?;
        file.seek(SeekFrom::Start(offset))
            .map_err(Error::io(path))?;
        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes).map_err(Error::io(path))?;
        // Only whole lines: a line still being written waits for the next
        // call.
        let Some(last_newline) = bytes.iter().rposition(|&byte| byte == b'\n') else {
            return Ok((Vec::new(), offset));
        };
        let whole = &bytes[..=last_newline];
        let mut lines: Vec<&[u8]> = whole[..last_newline].split(|&byte| byte == b'\n').collect();
        if offset == 0 && !lines.is_empty() {
            lines.remove(0);
        }
        let lines = lines
            .into_iter()
            .map(|line| {
                let line = line.strip_suffix(b"\r").unwrap_or(line);
                String::from_utf8_lossy(line).into_owned()
            })
            .collect();
        Ok((lines, offset + whole.len() as u64))
    }

    /// The common git directory this store is in.
    fn common_dir(&self) -> PathBuf {
        self.dir
            .parent()
            .and_then(Path::parent)
            .map_or_else(|| self.dir.clone(), Path::to_owned)
    }

    fn thread_path(&self, id: &AgentThreadId) -> PathBuf {
        self.thread_dir(id).join(THREAD_FILE)
    }
}

impl Run {
    /// Where the transcript is written.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The transcript's file name in `runs/`, for the turn's `transcript`.
    pub fn file_name(&self) -> String {
        self.path
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default()
    }

    /// Adds one line, such as a stream-JSON event of the agent. The line is
    /// written at once, so a crash loses nothing before it and a reader
    /// never sees half of it.
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

/// A transcript's header line, if this GitEZ understands its format.
fn parse_header(path: &Path, line: &str) -> Result<RunHeader> {
    let header: HeaderLine = serde_json::from_str(line).map_err(|source| Error::Invalid {
        path: path.to_owned(),
        source,
    })?;
    if header.version > VERSION {
        return Err(Error::TooNew {
            path: path.to_owned(),
            version: header.version,
        });
    }
    Ok(header.header)
}

/// `thread.json`'s content at `path`, if this GitEZ understands its format.
fn parse(path: &Path, text: &str) -> Result<AgentThread> {
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
