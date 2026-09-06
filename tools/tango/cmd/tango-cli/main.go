// Command tango-cli is a standalone, non-interactive driver for Uber's Tango
// (Target Analyzer in Go). Upstream Tango ships only a YARPC server example;
// this binary drives the same internals against a *local* git workspace and
// two revisions, and prints the changed targets as JSON.
//
// It lives outside the upstream repo (in cambium's tools/tango) and is copied
// into $DIR/src/cmd/tango-cli by fetch.sh so it can import the module's
// internal/... packages.
//
// Usage:
//
//	tango-cli -workspace <path> -base <sha> -head <sha> -bazel <bazel-binary> \
//	    [-startup-options "..."] [-extra-args "..."] [-exclude-external] -o out.json
package main

import (
	"bytes"
	"context"
	"encoding/json"
	"errors"
	"flag"
	"fmt"
	"os"
	"os/exec"
	"os/signal"
	"path/filepath"
	"sort"
	"strings"
	"syscall"
	"time"

	"github.com/uber/tango/config"
	"github.com/uber/tango/core/bazel"
	"github.com/uber/tango/core/git"
	"github.com/uber/tango/core/targethasher"
	"github.com/uber/tango/core/workspace"
	"github.com/uber/tango/entity"
	"github.com/uber/tango/graphrunner"
	"github.com/uber/tango/internal/targetdiff"
	"github.com/uber/tango/internal/tgb"
	"github.com/uber/tango/internal/tgbdiff"
	"github.com/uber/tango/mapper"
	"go.uber.org/zap"
)

type outputEntry struct {
	Label      string `json:"label"`
	Distance   int32  `json:"distance"`
	ChangeType string `json:"change_type"`
}

type options struct {
	workspace      string
	base           string
	head           string
	bazel          string
	startupOptions string
	extraArgs      string
	excludeExtern  bool
	out            string
	maxDistance    int
	queryTimeout   time.Duration
	scratch        string
	verbose        bool
	streamBazel    bool
}

func main() {
	if err := run(); err != nil {
		fmt.Fprintf(os.Stderr, "tango-cli: %v\n", err)
		os.Exit(1)
	}
}

func run() error {
	var o options
	flag.StringVar(&o.workspace, "workspace", ".", "path to the local git repository containing the Bazel workspace")
	flag.StringVar(&o.base, "base", "", "base revision (any git rev; the 'before' graph)")
	flag.StringVar(&o.head, "head", "", "head revision (any git rev; the 'after' graph)")
	flag.StringVar(&o.bazel, "bazel", "", "path to the bazel/bazelisk binary (required; Tango would otherwise download bazelisk)")
	flag.StringVar(&o.startupOptions, "startup-options", "", "Bazel startup options, placed before `query` (shell-style quoting)")
	flag.StringVar(&o.extraArgs, "extra-args", "", "extra `bazel query` args, placed after `query` (shell-style quoting)")
	flag.BoolVar(&o.excludeExtern, "exclude-external", false, "drop targets in external repositories from the output")
	flag.StringVar(&o.out, "o", "", "write JSON here instead of stdout")
	flag.IntVar(&o.maxDistance, "max-distance", -1, "limit reverse-dependency distance (-1 = unlimited)")
	flag.DurationVar(&o.queryTimeout, "query-timeout", 15*time.Minute, "per-revision bazel query timeout")
	flag.StringVar(&o.scratch, "scratch", "", "directory for the temporary git worktrees (default: os temp dir)")
	flag.BoolVar(&o.verbose, "v", false, "verbose logging to stderr")
	flag.BoolVar(&o.streamBazel, "stream-bazel-logs", false, "stream bazel stderr live instead of capturing it for errors")
	flag.Parse()

	if o.base == "" || o.head == "" {
		return errors.New("-base and -head are required")
	}
	if o.bazel == "" {
		return errors.New("-bazel is required (refusing to let Tango download bazelisk)")
	}
	bazelBin, err := exec.LookPath(o.bazel)
	if err != nil {
		return fmt.Errorf("-bazel %q: %w", o.bazel, err)
	}
	wsPath, err := filepath.Abs(o.workspace)
	if err != nil {
		return err
	}

	logger := zap.NewNop()
	if o.verbose {
		cfg := zap.NewDevelopmentConfig()
		cfg.OutputPaths = []string{"stderr"}
		cfg.ErrorOutputPaths = []string{"stderr"}
		if logger, err = cfg.Build(); err != nil {
			return err
		}
	}
	defer func() { _ = logger.Sync() }()

	ctx, stop := signal.NotifyContext(context.Background(), os.Interrupt, syscall.SIGTERM)
	defer stop()

	startupOptions, err := splitArgs(o.startupOptions)
	if err != nil {
		return fmt.Errorf("-startup-options: %w", err)
	}
	extraArgs, err := splitArgs(o.extraArgs)
	if err != nil {
		return fmt.Errorf("-extra-args: %w", err)
	}

	rootGit := git.New(wsPath, logger)
	baseSHA, err := rootGit.RevParse(ctx, o.base+"^{commit}")
	if err != nil {
		return fmt.Errorf("resolve -base %q: %w", o.base, err)
	}
	headSHA, err := rootGit.RevParse(ctx, o.head+"^{commit}")
	if err != nil {
		return fmt.Errorf("resolve -head %q: %w", o.head, err)
	}

	scratch := o.scratch
	if scratch == "" {
		scratch, err = os.MkdirTemp("", "tango-cli-")
		if err != nil {
			return err
		}
		defer func() { _ = os.RemoveAll(scratch) }()
	} else if err := os.MkdirAll(scratch, 0o755); err != nil {
		return err
	}

	// Two detached worktrees, one per revision: the user's checkout is never
	// touched, and each revision gets its own bazel workspace path (and hence
	// its own output base, unless -startup-options pins one).
	baseDir, cleanupBase, err := addWorktree(ctx, wsPath, filepath.Join(scratch, "base"), baseSHA)
	if err != nil {
		return err
	}
	defer cleanupBase()
	headDir, cleanupHead, err := addWorktree(ctx, wsPath, filepath.Join(scratch, "head"), headSHA)
	if err != nil {
		return err
	}
	defer cleanupHead()

	env := environMap()

	beforeRes, err := computeGraph(ctx, logger, computeParams{
		dir: baseDir, bazelBin: bazelBin, env: env,
		startupOptions: startupOptions, extraArgs: extraArgs,
		queryTimeout: o.queryTimeout, streamLogs: o.streamBazel,
	})
	if err != nil {
		return fmt.Errorf("compute base graph (%s): %w", baseSHA, err)
	}
	afterRes, err := computeGraph(ctx, logger, computeParams{
		dir: headDir, bazelBin: bazelBin, env: env,
		startupOptions: startupOptions, extraArgs: extraArgs,
		queryTimeout: o.queryTimeout, streamLogs: o.streamBazel,
	})
	if err != nil {
		return fmt.Errorf("compute head graph (%s): %w", headSHA, err)
	}

	before, err := encodeTGB(ctx, beforeRes)
	if err != nil {
		return fmt.Errorf("encode base graph: %w", err)
	}
	after, err := encodeTGB(ctx, afterRes)
	if err != nil {
		return fmt.Errorf("encode head graph: %w", err)
	}

	// Same comparison the production controller runs (compareTargetGraphsTGB).
	// SeedAttrs is nil: no RepositoryConfig.SeedAttributes allowlist, so every
	// string attribute counts as evidence of a direct change.
	diff, err := tgbdiff.Compare(ctx, before, after, tgbdiff.Options{
		MaxDistance: int32(o.maxDistance),
		SeedAttrs:   nil,
	})
	if err != nil {
		return fmt.Errorf("compare graphs: %w", err)
	}

	entries := make([]outputEntry, 0, len(diff.Changed))
	for _, c := range diff.Changed {
		if o.maxDistance >= 0 && (c.Distance < 0 || c.Distance > int32(o.maxDistance)) {
			continue
		}
		if o.excludeExtern && isExternal(before, after, c) {
			continue
		}
		entries = append(entries, outputEntry{
			Label:      c.Label,
			Distance:   c.Distance,
			ChangeType: changeTypeString(c.ChangeType),
		})
	}
	sort.Slice(entries, func(i, j int) bool {
		if entries[i].Distance != entries[j].Distance {
			return entries[i].Distance < entries[j].Distance
		}
		return entries[i].Label < entries[j].Label
	})

	buf, err := json.MarshalIndent(entries, "", "  ")
	if err != nil {
		return err
	}
	buf = append(buf, '\n')
	if o.out == "" {
		_, err = os.Stdout.Write(buf)
		return err
	}
	return os.WriteFile(o.out, buf, 0o644)
}

type computeParams struct {
	dir            string
	bazelBin       string
	env            map[string]string
	startupOptions []string
	extraArgs      []string
	queryTimeout   time.Duration
	streamLogs     bool
}

// computeGraph runs Tango's native graph runner over one checkout: it issues
// the exact `bazel query` the server issues, reads git ls-tree source hashes,
// and hashes the target graph.
func computeGraph(ctx context.Context, logger *zap.Logger, p computeParams) (targethasher.Result, error) {
	bzlmod := bzlmodEnabled(p.dir)
	bzlClient, err := bazel.NewBazelClient(ctx, bazel.Params{
		BazelCommand:  p.bazelBin,
		WorkspacePath: p.dir,
		// Tango replaces (not extends) the child environment when this map is
		// non-empty, so it must carry the full environment.
		EnvVarsMap:   p.env,
		Logger:       logger,
		QueryTimeout: p.queryTimeout,
		StreamLogs:   p.streamLogs,
	})
	if err != nil {
		return targethasher.EmptyResult(), err
	}
	gitClient := git.New(p.dir, logger)
	runner := graphrunner.NewNativeGraphRunner(graphrunner.NativeGraphRunnerParams{
		BazelClient: bzlClient,
		GitClient:   gitClient,
		Config: config.RepositoryConfig{
			BzlmodEnabled:       &bzlmod,
			BazelCommandPath:    p.bazelBin,
			BazelStartupOptions: p.startupOptions,
			BazelExtraArgs:      p.extraArgs,
			StreamBazelLogs:     p.streamLogs,
		},
	})
	ws := workspace.NewWorkspace(workspace.WorkspaceParams{
		Path:   p.dir,
		Git:    gitClient,
		Logger: logger,
	})
	defer func() { _ = ws.Release() }()
	return runner.Compute(ctx, ws)
}

// encodeTGB converts a hashed graph into the same TGB blob the server caches
// and serves, and returns a reader over it. Going through TGB (rather than
// diffing targethasher.Results directly) keeps this driver on the production
// comparison path.
func encodeTGB(ctx context.Context, res targethasher.Result) (*tgb.Reader, error) {
	targets, meta, err := mapper.ResultToTargetGraph(ctx, res)
	if err != nil {
		return nil, err
	}
	chunks := []entity.GetTargetGraphResponse{{Targets: targets, Metadata: meta}}
	var buf bytes.Buffer
	if err := tgbdiff.EncodeChunks(chunks, &buf); err != nil {
		return nil, err
	}
	return tgb.NewReader(buf.Bytes())
}

// isExternal reports whether a changed target belongs to an external
// repository, per the graph in which it exists.
func isExternal(before, after *tgb.Reader, c tgbdiff.ChangedTarget) bool {
	pkg, name := tgb.SplitLabelString(c.Label)
	readers := []*tgb.Reader{after, before}
	if c.ChangeType == targetdiff.ChangeTypeDeleted {
		readers = []*tgb.Reader{before, after}
	}
	for _, r := range readers {
		if node := r.FindNode(pkg, name); node >= 0 {
			return r.External(node)
		}
	}
	// Fall back to the label shape for anything we cannot locate.
	return strings.HasPrefix(c.Label, "@") || strings.HasPrefix(c.Label, "//external:")
}

func changeTypeString(ct targetdiff.ChangeType) string {
	switch ct {
	case targetdiff.ChangeTypeNew:
		return "new"
	case targetdiff.ChangeTypeDeleted:
		return "deleted"
	case targetdiff.ChangeTypeChanged:
		return "changed"
	default:
		return "invalid"
	}
}

// bzlmodEnabled mirrors Tango's RepositoryConfig.BzlmodEnabled, detected from
// the checkout: with bzlmod off the graph runner prepends
// `//external:all-targets +` to the query, which only resolves under legacy
// WORKSPACE external repos.
func bzlmodEnabled(dir string) bool {
	for _, name := range []string{"MODULE.bazel", "MODULE.bazel.lock"} {
		if _, err := os.Stat(filepath.Join(dir, name)); err == nil {
			return true
		}
	}
	return false
}

func environMap() map[string]string {
	env := make(map[string]string)
	for _, kv := range os.Environ() {
		if i := strings.IndexByte(kv, '='); i > 0 {
			env[kv[:i]] = kv[i+1:]
		}
	}
	return env
}

// addWorktree creates a detached worktree of rev under dir and returns a
// cleanup that removes it again.
func addWorktree(ctx context.Context, repo, dir, rev string) (string, func(), error) {
	if err := os.RemoveAll(dir); err != nil {
		return "", nil, err
	}
	cmd := exec.CommandContext(ctx, "git", "-C", repo, "worktree", "add", "--detach", "--quiet", dir, rev)
	cmd.Stderr = os.Stderr
	if err := cmd.Run(); err != nil {
		return "", nil, fmt.Errorf("git worktree add %s %s: %w", dir, rev, err)
	}
	cleanup := func() {
		rm := exec.Command("git", "-C", repo, "worktree", "remove", "--force", dir)
		rm.Stderr = os.Stderr
		_ = rm.Run()
	}
	return dir, cleanup, nil
}

// splitArgs splits a flag value into argv the way a shell would, honouring
// single and double quotes and backslash escapes. It is deliberately small:
// it exists so `-startup-options "--output_base=/path with space"` works.
func splitArgs(s string) ([]string, error) {
	var (
		args    []string
		cur     strings.Builder
		started bool
		quote   byte
	)
	for i := 0; i < len(s); i++ {
		c := s[i]
		switch {
		case quote != 0:
			if c == quote {
				quote = 0
				continue
			}
			if quote == '"' && c == '\\' && i+1 < len(s) {
				i++
				cur.WriteByte(s[i])
				continue
			}
			cur.WriteByte(c)
		case c == '\'' || c == '"':
			quote = c
			started = true
		case c == '\\' && i+1 < len(s):
			i++
			cur.WriteByte(s[i])
			started = true
		case c == ' ' || c == '\t' || c == '\n':
			if started {
				args = append(args, cur.String())
				cur.Reset()
				started = false
			}
		default:
			cur.WriteByte(c)
			started = true
		}
	}
	if quote != 0 {
		return nil, fmt.Errorf("unterminated %c quote in %q", quote, s)
	}
	if started {
		args = append(args, cur.String())
	}
	return args, nil
}
