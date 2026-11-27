//! A `gh` that asks Desk before it merges a pull request, first on the PATH
//! of a Codex session's app-server. Codex runs a command with no hook before
//! it, and the rules that could forbid one (`prefix_rule`, CLI 0.166) are
//! read from `CODEX_HOME/rules`, so they would hold every session alike;
//! Desk refuses a merge only while a boss acting fully has the session
//! ([`crate::Desk::gh_asked`]). Any other `gh` command runs the real one as
//! it was asked. A merge Desk can't be asked about doesn't run. It guards
//! against a session merging the plain way, not one set on it (an alias,
//! the real `gh` by its path, the API by `curl`): GitHub's own protection
//! of the branch is the boundary.

use std::path::{Path, PathBuf};

use eyre::{Context, Result};

/// The first `gh` on `path` that can be run.
fn real_gh(path: &str) -> Option<PathBuf> {
    use std::os::unix::fs::PermissionsExt;
    std::env::split_paths(path)
        .map(|dir| dir.join("gh"))
        .find(|gh| {
            std::fs::metadata(gh).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
        })
}

/// `text` as one word in a shell, in single quotes.
fn quoted(text: &str) -> String {
    format!("'{}'", text.replace('\'', r"'\''"))
}

/// The shim: `real` run as asked, but for a command spelled as a merge
/// (`gh pr merge`, `gh api` of a `…/merge` endpoint or a GraphQL
/// `mergePullRequest`), which is sent to `url` first, never through a
/// proxy, and runs only when Desk says nothing against it.
fn script(url: &str, real: &Path) -> String {
    let real = quoted(&real.to_string_lossy());
    let url = quoted(url);
    format!(
        r#"#!/bin/sh
# Desk's gh for this session: a merge of a pull request is asked of Desk first.
asks=
[ "$1" = pr ] && [ "$2" = merge ] && asks=1
if [ "$1" = api ]; then
  for a in "$@"; do
    case "$a" in
      */merge|*/merge/|*/merge\?*|*mergePullRequest*) asks=1 ;;
    esac
  done
fi
[ -n "$asks" ] || exec {real} "$@"
cmd=gh
for a in "$@"; do
  cmd="$cmd '$(printf '%s' "$a" | sed "s/'/'\\\\''/g")'"
done
said=$(printf '%s' "$cmd" | curl -sS --fail --noproxy '*' --max-time 10 -H 'Content-Type: text/plain' --data-binary @- {url}) || {{
  echo "gh: Desk couldn't be asked whether this may run, so it doesn't." >&2
  exit 1
}}
[ -z "$said" ] && exec {real} "$@"
printf 'gh: %s\n' "$said" >&2
exit 1
"#
    )
}

/// Writes the shim for a session into `dir`, asking `url`, in front of the
/// `gh` that `path` finds: the PATH it runs on, `dir` first. None when
/// `path` has no `gh`, so nothing merges through one.
pub(crate) fn install(dir: &Path, url: &str, path: &str) -> Result<Option<String>> {
    use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
    let Some(real) = real_gh(path) else {
        return Ok(None);
    };
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)
        .with_context(|| format!("making {}", dir.display()))?;
    let shim = dir.join("gh");
    let written = dir.join(".gh");
    {
        use std::io::Write;
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o700)
            .open(&written)
            .with_context(|| format!("writing {}", written.display()))?;
        f.write_all(script(url, &real).as_bytes())?;
    }
    std::fs::rename(&written, &shim).with_context(|| format!("placing {}", shim.display()))?;
    let paths = std::iter::once(dir.to_path_buf()).chain(std::env::split_paths(path));
    let joined = std::env::join_paths(paths).context("joining its PATH")?;
    Ok(Some(joined.to_string_lossy().into_owned()))
}

#[cfg(test)]
#[allow(
    clippy::disallowed_methods,
    reason = "stand-in servers for tests, not routes the hub serves"
)]
mod tests {
    use std::os::unix::fs::PermissionsExt;
    use std::process::Output;

    use axum::routing::post;
    use axum::Router;

    use super::*;

    /// A `gh` that says what it was run with.
    fn fake_gh(dir: &Path) -> PathBuf {
        let bin = dir.join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        let gh = bin.join("gh");
        std::fs::write(&gh, "#!/bin/sh\necho \"ran $*\"\n").unwrap();
        std::fs::set_permissions(&gh, std::fs::Permissions::from_mode(0o755)).unwrap();
        bin
    }

    /// Desk answering every command asked with `answer`; what it was asked
    /// kept in `asked`.
    async fn desk_saying(
        answer: &'static str,
    ) -> (String, std::sync::Arc<parking_lot::Mutex<Vec<String>>>) {
        let asked = std::sync::Arc::new(parking_lot::Mutex::new(Vec::new()));
        let kept = asked.clone();
        let app = Router::new().route(
            "/gh/s/t",
            post(move |body: String| {
                let kept = kept.clone();
                async move {
                    kept.lock().push(body);
                    answer
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let at = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        (format!("http://{at}/gh/s/t"), asked)
    }

    async fn run(path: &str, args: &[&str]) -> Output {
        tokio::process::Command::new("gh")
            .args(args)
            .env("PATH", path)
            .output()
            .await
            .unwrap()
    }

    fn said(o: &Output) -> (bool, String, String) {
        (
            o.status.success(),
            String::from_utf8_lossy(&o.stdout).trim().to_string(),
            String::from_utf8_lossy(&o.stderr).trim().to_string(),
        )
    }

    /// Any other command runs the real `gh` unasked; a merge runs it only
    /// once Desk, asked with the command whole, says nothing against it.
    #[tokio::test]
    async fn a_merge_runs_only_as_desk_says_and_anything_else_as_asked() {
        let dir = tempfile::tempdir().unwrap();
        let bin = fake_gh(dir.path());
        let system = format!("{}:/usr/bin:/bin", bin.display());

        let (url, asked) = desk_saying("").await;
        let path = install(&dir.path().join("allows"), &url, &system)
            .unwrap()
            .expect("a gh to stand in front of");
        assert!(path.starts_with(&dir.path().join("allows").to_string_lossy().into_owned()));
        assert_eq!(
            said(&run(&path, &["pr", "view", "7"]).await),
            (true, "ran pr view 7".into(), String::new())
        );
        for args in [
            &["pr", "list", "--search", "merged"][..],
            &["pr", "view", "7", "--json", "mergeable,mergeStateStatus"],
            &["api", "repos/acme/web/pulls/7/merges"],
            &["merge"],
        ] {
            assert!(said(&run(&path, args).await).0, "{args:?}");
        }
        assert!(asked.lock().is_empty(), "nothing asked of what can't merge");
        for args in [
            &["api", "-X", "PUT", "repos/acme/web/pulls/7/merge"][..],
            &[
                "api",
                "graphql",
                "-f",
                "query=mutation { mergePullRequest(input: {}) { clientMutationId } }",
            ],
        ] {
            assert!(said(&run(&path, args).await).0, "{args:?}");
        }
        assert_eq!(asked.lock().len(), 2, "each merge spelling asked");
        asked.lock().clear();
        assert_eq!(
            said(&run(&path, &["pr", "merge", "7", "--body", "it's done"]).await),
            (
                true,
                "ran pr merge 7 --body it's done".into(),
                String::new()
            )
        );
        assert_eq!(
            asked.lock().clone(),
            vec![r#"gh 'pr' 'merge' '7' '--body' 'it'\''s done'"#.to_string()],
            "the command whole, quoted as a shell reads it"
        );
        assert!(crate::computer::boss::merges_a_pull(&asked.lock()[0]));

        let (url, _) = desk_saying("Your boss merges it.").await;
        let path = install(&dir.path().join("refuses"), &url, &system)
            .unwrap()
            .unwrap();
        assert_eq!(
            said(&run(&path, &["pr", "merge", "7", "--squash"]).await),
            (false, String::new(), "gh: Your boss merges it.".into())
        );

        let path = install(
            &dir.path().join("unasked"),
            "http://127.0.0.1:9/gh/s/t",
            &system,
        )
        .unwrap()
        .unwrap();
        let (ran, out, why) = said(&run(&path, &["pr", "merge", "7"]).await);
        assert!(!ran && out.is_empty(), "{out}");
        assert!(
            why.ends_with("Desk couldn't be asked whether this may run, so it doesn't."),
            "{why}"
        );

        assert_eq!(
            install(&dir.path().join("none"), &url, "/nowhere").unwrap(),
            None,
            "no gh, nothing in front of it"
        );
    }
}
