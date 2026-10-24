//! Guessing the command that runs a project's tests from the files at the
//! root of its working tree, for an agent to run after its changes.

use std::path::Path;

/// What `npm init` puts in `scripts.test`: not a test suite.
const NPM_PLACEHOLDER: &str = "no test specified";

/// The command that most likely runs the tests of the project in `dir`, or
/// `None` when nothing there says. Reads a few files; run it off the UI
/// thread.
pub fn detect_test_command(dir: &Path) -> Option<String> {
    let has = |name: &str| dir.join(name).is_file();
    let read = |name: &str| std::fs::read_to_string(dir.join(name)).ok();

    // A `test` target is the project's own answer.
    if let Some(makefile) = read("Makefile").or_else(|| read("makefile"))
        && makefile
            .lines()
            .any(|line| line.starts_with("test:") || line.starts_with("test :"))
    {
        return Some("make test".into());
    }
    if let Some(manifest) = read("Cargo.toml") {
        let workspace = manifest.lines().any(|line| line.trim() == "[workspace]");
        return Some(if workspace {
            "cargo test --workspace".into()
        } else {
            "cargo test".into()
        });
    }
    if let Some(package) = read("package.json") {
        let test = serde_json::from_str::<serde_json::Value>(&package)
            .ok()
            .and_then(|package| {
                package
                    .get("scripts")?
                    .get("test")?
                    .as_str()
                    .map(str::to_owned)
            });
        if test.is_some_and(|test| !test.contains(NPM_PLACEHOLDER)) {
            let runner = if has("pnpm-lock.yaml") {
                "pnpm test"
            } else if has("yarn.lock") {
                "yarn test"
            } else if has("bun.lock") || has("bun.lockb") {
                "bun run test"
            } else {
                "npm test"
            };
            return Some(runner.into());
        }
    }
    if has("go.mod") {
        return Some("go test ./...".into());
    }
    if has("pyproject.toml") || has("pytest.ini") || has("setup.py") || has("tox.ini") {
        return Some(if has("uv.lock") {
            "uv run pytest".into()
        } else {
            "pytest".into()
        });
    }
    if has("mix.exs") {
        return Some("mix test".into());
    }
    if has("gradlew") && (has("build.gradle") || has("build.gradle.kts")) {
        return Some("./gradlew test".into());
    }
    if has("pom.xml") {
        return Some("mvn test".into());
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn project(files: &[(&str, &str)]) -> tempfile::TempDir {
        let dir = tempfile::tempdir().expect("temporary directory");
        for (name, contents) in files {
            std::fs::write(dir.path().join(name), contents).expect("write");
        }
        dir
    }

    fn detect(files: &[(&str, &str)]) -> Option<String> {
        detect_test_command(project(files).path())
    }

    #[test]
    fn the_command_follows_the_project_files() {
        assert_eq!(detect(&[]), None);
        assert_eq!(
            detect(&[("Cargo.toml", "[package]\nname = \"a\"\n")]).as_deref(),
            Some("cargo test")
        );
        assert_eq!(
            detect(&[("Cargo.toml", "[workspace]\nmembers = []\n")]).as_deref(),
            Some("cargo test --workspace")
        );
        assert_eq!(
            detect(&[
                ("package.json", r#"{"scripts":{"test":"vitest"}}"#),
                ("pnpm-lock.yaml", "")
            ])
            .as_deref(),
            Some("pnpm test")
        );
        assert_eq!(
            detect(&[("package.json", r#"{"scripts":{"test":"jest"}}"#)]).as_deref(),
            Some("npm test")
        );
        // The placeholder of `npm init` runs no tests.
        let placeholder = r#"{"scripts":{"test":"echo \"Error: no test specified\" && exit 1"}}"#;
        assert_eq!(detect(&[("package.json", placeholder)]), None);
        assert_eq!(
            detect(&[("go.mod", "module a\n")]).as_deref(),
            Some("go test ./...")
        );
        assert_eq!(
            detect(&[("pyproject.toml", ""), ("uv.lock", "")]).as_deref(),
            Some("uv run pytest")
        );
        // A Makefile's test target wins over the language's default.
        assert_eq!(
            detect(&[
                ("Cargo.toml", "[package]\n"),
                (
                    "Makefile",
                    "build:\n\tcargo build\ntest: build\n\tcargo nextest run\n"
                )
            ])
            .as_deref(),
            Some("make test")
        );
    }
}
