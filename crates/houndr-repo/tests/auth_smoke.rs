// Network + auth smoke tests for git2 transport features.
//
// Marked `#[ignore]` because they require network access (and SSH needs a
// github-registered key in ssh-agent). CI doesn't run them.
//
// Run locally with:
//   cargo test -p houndr-repo --test auth_smoke -- --ignored --nocapture
//
// These exist to catch regressions like the git2 0.21 default-feature change,
// where the Rust API for Cred::* stays compilable but libgit2-sys is built
// without GIT_SSH / GIT_HTTPS — meaning real fetches silently fail.

use git2::{FetchOptions, RemoteCallbacks, Repository};
use std::path::PathBuf;

fn temp_path(tag: &str) -> PathBuf {
    let mut p = std::env::temp_dir();
    p.push(format!("houndr-auth-smoke-{}-{}", tag, std::process::id()));
    let _ = std::fs::remove_dir_all(&p);
    p
}

fn fetch_master(url: &str, callbacks: RemoteCallbacks<'_>, tag: &str) {
    let path = temp_path(tag);
    let repo = Repository::init_bare(&path).expect("init bare");
    let mut remote = repo.remote("origin", url).expect("add remote");

    let mut fo = FetchOptions::new();
    fo.remote_callbacks(callbacks);

    let result = remote.fetch(&["master"], Some(&mut fo), None);
    let _ = std::fs::remove_dir_all(&path);
    result.unwrap_or_else(|e| panic!("{tag} fetch failed: {e}"));
}

#[test]
#[ignore = "network: HTTPS clone smoke test"]
fn https_clone_works() {
    let mut callbacks = RemoteCallbacks::new();
    callbacks.certificate_check(|_, _| Ok(git2::CertificateCheckStatus::CertificatePassthrough));

    fetch_master(
        "https://github.com/octocat/Hello-World.git",
        callbacks,
        "https",
    );
}

#[test]
#[ignore = "network + ssh-agent: SSH clone smoke test"]
fn ssh_clone_works() {
    let mut callbacks = RemoteCallbacks::new();
    callbacks.certificate_check(|cert, _host| {
        if cert.as_hostkey().is_some() {
            Ok(git2::CertificateCheckStatus::CertificateOk)
        } else {
            Ok(git2::CertificateCheckStatus::CertificatePassthrough)
        }
    });
    callbacks.credentials(|_url, username, _allowed| {
        git2::Cred::ssh_key_from_agent(username.unwrap_or("git"))
    });

    fetch_master("git@github.com:octocat/Hello-World.git", callbacks, "ssh");
}
