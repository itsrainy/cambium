package main

// Copied into $DIR/src/cmd/tango-cli by fetch.sh next to main.go and run
// there with `go test ./cmd/tango-cli`; it needs only git.

import (
	"context"
	"os"
	"os/exec"
	"path/filepath"
	"testing"
)

func sh(t *testing.T, dir string, args ...string) string {
	t.Helper()
	cmd := exec.Command(args[0], args[1:]...)
	cmd.Dir = dir
	out, err := cmd.CombinedOutput()
	if err != nil {
		t.Fatalf("%v in %s: %v\n%s", args, dir, err, out)
	}
	return string(out)
}

// A repository with one commit, under a canonicalised temp dir: git records
// worktree paths with symlinks resolved, and on macOS $TMPDIR is one.
func repoWithCommit(t *testing.T) (string, string) {
	t.Helper()
	root, err := filepath.EvalSymlinks(t.TempDir())
	if err != nil {
		t.Fatal(err)
	}
	repo := filepath.Join(root, "repo")
	if err := os.MkdirAll(repo, 0o755); err != nil {
		t.Fatal(err)
	}
	sh(t, repo, "git", "init", "--quiet", "-b", "main")
	sh(t, repo, "git", "config", "user.email", "t@example.com")
	sh(t, repo, "git", "config", "user.name", "t")
	if err := os.WriteFile(filepath.Join(repo, "a.txt"), []byte("one"), 0o644); err != nil {
		t.Fatal(err)
	}
	sh(t, repo, "git", "add", "-A")
	sh(t, repo, "git", "commit", "--quiet", "-m", "one")
	return root, repo
}

// The common case: nothing is registered, so there is nothing to recover
// and — the point — nothing to say. The first published run's apollo
// failures were misread because the recovery announced itself on a clean
// registry with four `is not a working tree` lines.
func TestRecoveryIsSilentOnACleanRegistry(t *testing.T) {
	root, repo := repoWithCommit(t)
	dir := filepath.Join(root, "scratch", "tango-worktrees", "base")
	if recoverStaleWorktree(context.Background(), repo, dir) {
		t.Fatalf("recovered a worktree that was never registered: %s", dir)
	}
}

// What a killed run leaves behind: registered, locked, gone from disk. `git
// worktree add` refuses that outright, so addWorktree must clear it first.
func TestALockedMissingWorktreeIsRecoveredAndReadded(t *testing.T) {
	root, repo := repoWithCommit(t)
	rev := sh(t, repo, "git", "rev-parse", "HEAD")
	dir := filepath.Join(root, "scratch", "tango-worktrees", "base")
	sh(t, repo, "git", "worktree", "add", "--detach", "--quiet", dir, "HEAD")
	sh(t, repo, "git", "worktree", "lock", dir)
	if err := os.RemoveAll(filepath.Dir(dir)); err != nil {
		t.Fatal(err)
	}
	if !worktreeRegistered(context.Background(), repo, dir) {
		t.Fatalf("the stale registration must still be there for this test to mean anything")
	}

	got, cleanup, err := addWorktree(context.Background(), repo, dir, rev[:40])
	if err != nil {
		t.Fatalf("addWorktree over a locked, missing registration: %v", err)
	}
	defer cleanup()
	if got != dir {
		t.Fatalf("addWorktree returned %s, want %s", got, dir)
	}
	if _, err := os.Stat(filepath.Join(dir, "a.txt")); err != nil {
		t.Fatalf("the re-added worktree is not checked out: %v", err)
	}
}
