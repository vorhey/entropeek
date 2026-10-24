//! End-to-end tests: run the real binary against a fake home directory.

use std::path::Path;
use std::process::{Command, Output, Stdio};

use entropeek::env::Env;
use entropeek::testkit;

struct Home {
    dir: tempfile::TempDir,
    env: Env,
    ids: Vec<String>,
}

impl Home {
    fn new() -> Home {
        let dir = tempfile::tempdir().unwrap();
        let env = Env::under(dir.path());
        let ids = testkit::populate(&env);
        Home { dir, env, ids }
    }

    fn run(&self, args: &[&str]) -> Output {
        self.run_with_path(args, "/usr/bin:/bin")
    }

    fn run_with_path(&self, args: &[&str], path: &str) -> Output {
        self.command(args, path).output().expect("run entropeek")
    }

    fn command(&self, args: &[&str], path: &str) -> Command {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_entropeek"));
        cmd.args(args)
            .env_clear()
            .env("HOME", self.dir.path())
            .env("PATH", path)
            .env("CLAUDE_CONFIG_DIR", &self.env.claude_dir)
            .env("CODEX_HOME", &self.env.codex_dir)
            .env("ENTROPEEK_CACHE_DIR", &self.env.cache_dir)
            .env("ENTROPEEK_DATA_DIR", &self.env.data_dir)
            .stdin(Stdio::null());
        cmd
    }

    fn ok(&self, args: &[&str]) -> String {
        let out = self.run(args);
        assert!(out.status.success(), "{args:?} failed: {}", String::from_utf8_lossy(&out.stderr));
        String::from_utf8(out.stdout).unwrap()
    }

    fn fails(&self, args: &[&str]) -> String {
        let out = self.run(args);
        assert!(!out.status.success(), "{args:?} unexpectedly succeeded");
        String::from_utf8(out.stderr).unwrap()
    }

    fn json(&self, args: &[&str]) -> serde_json::Value {
        serde_json::from_str(&self.ok(args)).expect("valid json")
    }
}

#[test]
fn list_prints_every_session_newest_first() {
    let home = Home::new();
    let out = home.ok(&["list"]);
    let lines: Vec<&str> = out.lines().collect();
    assert_eq!(lines.len(), 5, "{out}");
    assert!(lines[0].starts_with("AGENT"));
    assert!(lines[1].contains("codex") && lines[1].contains("Review rate limiter"));
    assert!(lines[2].contains("fix the flaky redis test"));
    assert!(lines[3].contains("landing-page"));
    assert!(lines[4].contains("Rate limiting for API"));
}

#[test]
fn no_subcommand_without_a_terminal_lists() {
    let home = Home::new();
    assert_eq!(home.ok(&[]), home.ok(&["list"]));
}

#[test]
fn list_json_has_stable_fields() {
    let home = Home::new();
    let rows = home.json(&["list", "--json"]);
    let rows = rows.as_array().unwrap();
    assert_eq!(rows.len(), 4);
    let first = &rows[0];
    assert_eq!(first["id"], home.ids[0].as_str());
    assert_eq!(first["agent"], "codex");
    assert_eq!(first["cwd"], "/work/app");
    assert_eq!(first["title"], "Review rate limiter");
    assert_eq!(first["first_prompt"], "review the rate limiter");
    assert_eq!(first["updated_at"], "2026-10-04T09:00:30Z");
    assert_eq!(first["git_branch"], "main");
    assert_eq!(first["user_turns"], 1);
    assert_eq!(first["resume_command"], format!("cd /work/app && codex resume {}", home.ids[0]));
}

#[test]
fn list_filters_by_agent_query_and_limit() {
    let home = Home::new();
    let ids = |args: &[&str]| -> Vec<String> {
        home.json(args).as_array().unwrap().iter().map(|r| r["id"].as_str().unwrap().to_string()).collect()
    };
    assert_eq!(ids(&["list", "--json", "--agent", "codex"]), vec![home.ids[0].clone()]);
    assert_eq!(ids(&["list", "--json", "-q", "RATE limit"]), vec![home.ids[0].clone(), home.ids[3].clone()]);
    assert_eq!(ids(&["list", "--json", "-l", "2"]).len(), 2);
    assert!(home.fails(&["list", "--agent", "gemini"]).contains("unknown agent"));
}

#[test]
fn show_prints_the_conversation() {
    let home = Home::new();
    let out = home.ok(&["show", "latest"]);
    assert!(out.contains("Codex · Review rate limiter"), "{out}");
    assert!(out.contains("── you ──\nreview the rate limiter"));
    assert!(out.contains("── assistant ──\nLooks good overall."));
    assert!(!out.contains("environment_context"));

    let json = home.json(&["show", &home.ids[3], "--json"]);
    assert_eq!(json["session"]["title"], "Rate limiting for API");
    let turns = json["turns"].as_array().unwrap();
    assert_eq!(turns.len(), 2);
    assert_eq!(turns[0]["role"], "user");
    assert_eq!(turns[1]["text"], "Sure, adding a token bucket.");

    let last = home.json(&["show", &home.ids[3], "--json", "--last", "1"]);
    assert_eq!(last["turns"].as_array().unwrap().len(), 1);
}

#[test]
fn unknown_or_ambiguous_ids_fail_clearly() {
    let home = Home::new();
    assert!(home.fails(&["show", "zzzzzzzz"]).contains("no session"));
    assert!(home.fails(&["show", "aaaaaaaa"]).contains("matches 3 sessions"));
}

#[test]
fn resume_print_and_missing_program() {
    let home = Home::new();
    let line = home.ok(&["resume", &home.ids[1], "--print"]);
    assert_eq!(line.trim(), format!("cd /work/app && claude --resume {}", home.ids[1]));
    // /work/app does not exist in the test environment.
    let err = home.fails(&["resume", &home.ids[1]]);
    assert!(err.contains("no longer exists"), "{err}");
}

#[test]
fn resume_execs_the_agent_in_the_workspace() {
    let home = Home::new();
    let workspace = home.dir.path().join("ws");
    std::fs::create_dir_all(&workspace).unwrap();
    let id = "bbbbbbbb-0000-0000-0000-000000000009";
    let ws = workspace.to_str().unwrap();
    testkit::claude::write_session(
        &home.env.claude_dir,
        id,
        ws,
        &[testkit::claude::user(id, ws, "2026-10-09T00:00:00Z", "hi")],
    );
    // A fake `claude` that reports how it was called.
    let bin = home.dir.path().join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    write_script(&bin.join("claude"), "#!/bin/sh\necho \"pwd=$(pwd) args=$*\"\n");

    let out = home.run_with_path(&["resume", "latest"], &format!("{}:/usr/bin:/bin", bin.display()));
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let stdout = String::from_utf8(out.stdout).unwrap();
    let canonical = workspace.canonicalize().unwrap();
    assert_eq!(stdout.trim(), format!("pwd={} args=--resume {id}", canonical.display()));
}

#[test]
fn delete_needs_confirmation_and_can_be_undone() {
    let home = Home::new();
    let target = &home.ids[2];
    assert!(home.fails(&["delete", target]).contains("--yes"));
    assert!(home.fails(&["delete", "latest", "--yes"]).contains("explicit session id"));
    assert_eq!(home.json(&["list", "--json"]).as_array().unwrap().len(), 4);

    let out = home.ok(&["delete", target, "--yes"]);
    assert!(out.contains("moved claude session"), "{out}");
    let remaining = home.json(&["list", "--json"]);
    assert!(remaining.as_array().unwrap().iter().all(|r| r["id"] != target.as_str()));

    let trash = home.json(&["trash", "list", "--json"]);
    assert_eq!(trash.as_array().unwrap().len(), 1);
    assert_eq!(trash[0]["session"]["id"], target.as_str());
    assert!(home.ok(&["trash"]).contains("landing-page"));

    assert!(home.ok(&["trash", "restore", target]).contains("restored claude session"));
    assert_eq!(home.json(&["list", "--json"]).as_array().unwrap().len(), 4);
    assert!(home.ok(&["trash"]).contains("the trash is empty"));
}

#[test]
fn purge_empties_the_trash() {
    let home = Home::new();
    home.ok(&["delete", &home.ids[0], "--yes"]);
    assert!(home.fails(&["trash", "purge"]).contains("exactly one"));
    assert!(home.ok(&["trash", "purge", "--older-than-days", "1"]).contains("deleted 0"));
    assert!(home.ok(&["trash", "purge", "--all"]).contains("deleted 1"));
    assert_eq!(home.json(&["trash", "list", "--json"]).as_array().unwrap().len(), 0);
}

#[test]
fn cache_is_written_and_no_cache_still_works() {
    let home = Home::new();
    let cache = home.env.cache_dir.join("index.bin");
    assert!(!cache.exists());
    let first = home.ok(&["list", "--json"]);
    assert!(cache.exists());
    assert_eq!(home.ok(&["list", "--json"]), first);
    assert_eq!(home.ok(&["--no-cache", "list", "--json"]), first);
}

#[test]
fn whitespace_does_not_bypass_the_latest_guard() {
    let home = Home::new();
    for selector in [" latest ", "latest\n", "\tlatest"] {
        assert!(home.fails(&["delete", selector, "--yes"]).contains("explicit session id"), "{selector:?}");
    }
    assert_eq!(home.json(&["list", "--json"]).as_array().unwrap().len(), 4);
}

#[test]
fn duplicate_ids_must_be_qualified() {
    let home = Home::new();
    // A Claude session file named with the Codex session's id.
    let id = home.ids[0].clone();
    testkit::claude::write_session(
        &home.env.claude_dir,
        &id,
        "/work/dup",
        &[testkit::claude::user(&id, "/work/dup", "2026-10-01T00:00:00Z", "same id")],
    );
    let err = home.fails(&["show", &id]);
    assert!(err.contains("matches 2 sessions"), "{err}");
    assert!(home.fails(&["delete", &id, "--yes"]).contains("matches 2 sessions"));
    assert!(home.ok(&["show", &format!("claude:{id}")]).contains("same id"));
    assert!(home.ok(&["show", &format!("codex:{id}")]).contains("review the rate limiter"));
    assert_eq!(home.json(&["list", "--json"]).as_array().unwrap().len(), 5, "nothing was deleted");
}

#[test]
fn table_ids_are_long_enough_to_use() {
    let home = Home::new();
    // The three Claude fixture ids share their first 35 characters.
    let out = home.ok(&["list"]);
    for id in &home.ids {
        assert!(out.contains(id.as_str()), "{id} not shown in full:\n{out}");
    }
}

#[test]
fn terminal_controls_in_metadata_are_not_printed() {
    let home = Home::new();
    let id = "cccccccc-0000-0000-0000-000000000001";
    let cwd = "/work/\u{1b}]52;c;aGk=\u{7}evil\u{1b}[2J";
    testkit::claude::write_session(
        &home.env.claude_dir,
        id,
        "/work/x",
        &[testkit::claude::user(id, cwd, "2026-10-09T00:00:00Z", "hi")],
    );
    for args in [vec!["list"], vec!["show", id], vec!["resume", id, "--print"]] {
        let out = home.run(&args);
        let all = [out.stdout, out.stderr].concat();
        assert!(!all.contains(&0x1b) && !all.contains(&0x07), "{args:?}: {:?}", String::from_utf8_lossy(&all));
    }
}

#[test]
fn relative_config_dirs_restore_to_the_same_place() {
    let home = Home::new();
    let a = home.dir.path().join("a");
    let b = home.dir.path().join("b");
    std::fs::create_dir_all(&b).unwrap();
    // Move the fixture's Claude data under a/claude and point at it relatively.
    std::fs::create_dir_all(&a).unwrap();
    std::fs::rename(&home.env.claude_dir, a.join("claude")).unwrap();
    let run = |cwd: &Path, args: &[&str]| {
        let out =
            home.command(args, "/usr/bin:/bin").current_dir(cwd).env("CLAUDE_CONFIG_DIR", "claude").output().unwrap();
        assert!(out.status.success(), "{args:?}: {}", String::from_utf8_lossy(&out.stderr));
        String::from_utf8(out.stdout).unwrap()
    };
    let target = &home.ids[2];
    run(&a, &["delete", target, "--yes"]);
    let file = testkit::claude::session_path(&a.join("claude"), target, "/work/site");
    assert!(!file.exists());
    run(&b, &["trash", "restore", target]);
    assert!(file.exists(), "restored under a/claude, where it came from");
    assert!(!b.join("claude").exists(), "nothing was created under b");
}

#[test]
fn relative_path_entries_cannot_swap_the_program() {
    let home = Home::new();
    let workspace = home.dir.path().join("ws");
    let launcher = home.dir.path().join("launcher");
    std::fs::create_dir_all(workspace.join("bin")).unwrap();
    std::fs::create_dir_all(launcher.join("bin")).unwrap();
    write_script(&launcher.join("bin/claude"), "#!/bin/sh\necho validated\n");
    write_script(&workspace.join("bin/claude"), "#!/bin/sh\necho workspace-program\n");
    let id = "bbbbbbbb-0000-0000-0000-000000000009";
    let ws = workspace.to_str().unwrap();
    testkit::claude::write_session(
        &home.env.claude_dir,
        id,
        ws,
        &[testkit::claude::user(id, ws, "2026-10-09T00:00:00Z", "hi")],
    );
    let out = home.command(&["resume", id], "bin:/usr/bin:/bin").current_dir(&launcher).output().unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    assert_eq!(String::from_utf8(out.stdout).unwrap().trim(), "validated");
}

#[test]
fn codex_subagents_are_part_of_their_session() {
    let home = Home::new();
    let parent = home.ids[0].clone();
    let child = "01a16f5e-e564-7780-be78-5227d75ea51b";
    let child_file = testkit::codex::write_session(
        &home.env.codex_dir,
        child,
        &[
            testkit::codex::subagent_meta(child, &parent, "/work/app", "2026-10-04T09:01:00Z"),
            testkit::codex::meta(&parent, "/work/app", "2026-10-04T09:00:00Z"),
            testkit::codex::user("2026-10-04T09:00:10Z", "review the rate limiter"),
        ],
    );
    let listed = home.json(&["list", "--json"]);
    assert_eq!(listed.as_array().unwrap().len(), 4, "the sub-agent is not listed as a session");
    assert!(listed.as_array().unwrap().iter().all(|s| s["id"] != child));

    home.ok(&["delete", &parent, "--yes"]);
    assert!(!child_file.exists(), "the sub-agent's file went to the trash with its session");
    home.ok(&["trash", "restore", &parent]);
    assert!(child_file.exists(), "and came back with it");
    assert_eq!(home.json(&["list", "--json"]).as_array().unwrap().len(), 4);
}

fn write_script(path: &Path, body: &str) {
    std::fs::write(path, body).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
}
