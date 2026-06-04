/**
 * Tool handler functions for the ws-ckpt OpenClaw plugin.
 *
 * Each handle* function implements the business logic for one tool.
 * They access shared state via the pluginState singleton.
 */

import { CommandExecutor } from "./commands.js";
import { mapErrorToLLMMessage } from "./btrfs-manager.js";
import type { AgentToolResult } from "../types-shim.js";
import { pluginState, UNAVAILABLE_MSG, cwdInsideWorkspace, cwdInsideWorkspaceReason } from "./state.js";
import { parseWorkspaceCleanupJson } from "./config.js";

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

export function textToolResult(
  text: string,
  isError?: boolean,
): AgentToolResult {
  return {
    content: [{ type: "text", text }],
    details: isError ? { status: "failed" } : undefined,
  };
}

// ---------------------------------------------------------------------------
// Handler functions
// ---------------------------------------------------------------------------

export async function handleCheckpoint(
  argsStr?: string,
): Promise<{ text: string; isError: boolean }> {
  if (!pluginState.manager || !pluginState.environmentReady) {
    return { text: UNAVAILABLE_MSG, isError: true };
  }
  // pin is no longer exposed to plugin users (auto-cleanup disabled).
  const args = argsStr ? JSON.parse(argsStr) : {};
  const id = args.id;
  if (!id) {
    return { text: "Missing required parameter: id", isError: true };
  }
  const message = args.message?.trim() || "manual checkpoint";
  const explicitWs = (args.workspace as string | undefined)?.trim();

  // Explicit workspace bypasses the manager (and its workspace-bound cache),
  // mirroring the handleDelete pattern.
  if (explicitWs) {
    const cwdCheck = cwdInsideWorkspace(explicitWs);
    if (cwdCheck.inside) {
      return { text: cwdInsideWorkspaceReason(cwdCheck.cwd, explicitWs), isError: true };
    }
    try {
      const executor = new CommandExecutor();
      const output = await executor.checkpoint(explicitWs, id, { message });
      if (output.exitCode !== 0) {
        return { text: mapErrorToLLMMessage(output.stderr, { id }), isError: true };
      }
      if (output.stdout && (output.stdout.includes('Skipped') || output.stdout.includes('Empty workspace'))) {
        return { text: 'Empty workspace, no snapshot created.', isError: false };
      }
      return { text: `Checkpoint created: ${id}`, isError: false };
    } catch (error) {
      const msg = error instanceof Error ? error.message : String(error);
      return { text: `Checkpoint error: ${msg}`, isError: true };
    }
  }

  const ws = pluginState.resolvedConfig?.workspace;
  if (ws) {
    const cwdCheck = cwdInsideWorkspace(ws);
    if (cwdCheck.inside) {
      return { text: cwdInsideWorkspaceReason(cwdCheck.cwd, ws), isError: true };
    }
  }

  const result = await pluginState.manager.createCheckpoint({
    id,
    message,
  });
  if (result.skipped) {
    return { text: result.reason ?? "Empty workspace, no snapshot created.", isError: false };
  }
  return { text: result.message, isError: !result.success };
}

export async function handleRollback(
  target?: string,
  workspace?: string,
): Promise<{ text: string; isError: boolean }> {
  if (!pluginState.manager || !pluginState.environmentReady) {
    return { text: UNAVAILABLE_MSG, isError: true };
  }
  const trimmed = target?.trim();
  if (!trimmed) {
    return {
      text: "Usage: ws-ckpt-rollback <target>\n  target: snapshot hash id",
      isError: true,
    };
  }

  const explicitWs = workspace?.trim();
  if (explicitWs) {
    const cwdCheck = cwdInsideWorkspace(explicitWs);
    if (cwdCheck.inside) {
      return { text: cwdInsideWorkspaceReason(cwdCheck.cwd, explicitWs), isError: true };
    }
    try {
      const executor = new CommandExecutor();
      const output = await executor.rollback(explicitWs, trimmed);
      if (output.exitCode !== 0) {
        return { text: mapErrorToLLMMessage(output.stderr, { id: trimmed }), isError: true };
      }
      return { text: `Rolled back to ${trimmed}`, isError: false };
    } catch (error) {
      const msg = error instanceof Error ? error.message : String(error);
      return { text: `Rollback error: ${msg}`, isError: true };
    }
  }

  const ws = pluginState.resolvedConfig?.workspace;
  if (ws) {
    const cwdCheck = cwdInsideWorkspace(ws);
    if (cwdCheck.inside) {
      return { text: cwdInsideWorkspaceReason(cwdCheck.cwd, ws), isError: true };
    }
  }

  const result = await pluginState.manager.rollback(trimmed);
  return { text: result.message, isError: !result.success };
}

export async function handleListCheckpoints(): Promise<{
  text: string;
  isError: boolean;
}> {
  if (!pluginState.manager || !pluginState.environmentReady) {
    return { text: UNAVAILABLE_MSG, isError: true };
  }
  const checkpoints = await pluginState.manager.listCheckpoints();
  if (checkpoints.length === 0) {
    return {
      text: "No checkpoints found. The workspace is active and daemon is responding \u2014 there are simply no snapshots yet.",
      isError: false,
    };
  }
  const header = ["ID", "Created At", "Message", "Metadata"];
  const rows = checkpoints.map((cp) => [
    cp.snapshot,
    cp.createdAt,
    cp.message ?? "",
    cp.metadata ? JSON.stringify(cp.metadata) : "",
  ]);
  const widths = header.map((h, i) =>
    Math.max(h.length, ...rows.map((r) => r[i].length)),
  );
  const fmt = (cols: string[]) =>
    cols.map((c, i) => c.padEnd(widths[i])).join("  ");
  const lines: string[] = [
    `Checkpoints (${checkpoints.length}):`,
    "",
    fmt(header),
    widths.map((w) => "-".repeat(w)).join("  "),
    ...rows.map(fmt),
  ];
  return { text: lines.join("\n"), isError: false };
}

export async function handleDelete(
  snapshot?: string,
  workspace?: string,
): Promise<{ text: string; isError: boolean }> {
  if (!pluginState.manager || !pluginState.environmentReady) {
    return { text: UNAVAILABLE_MSG, isError: true };
  }
  if (!snapshot) {
    return {
      text: "Usage: ws-ckpt-delete <snapshot> [workspace]\n  snapshot: snapshot ID to delete (required)\n  workspace: workspace path (optional, defaults to current)",
      isError: true,
    };
  }
  const ws = workspace || pluginState.resolvedConfig?.workspace;
  if (!ws) {
    return { text: "No workspace path available", isError: true };
  }
  try {
    const executor = new CommandExecutor();
    const output = await executor.delete(snapshot, { workspace: ws, force: true });
    if (output.exitCode !== 0) {
      return { text: mapErrorToLLMMessage(output.stderr, { id: snapshot }), isError: true };
    }
    pluginState.manager.getStore().remove(snapshot);
    return { text: `Snapshot ${snapshot} deleted`, isError: false };
  } catch (error) {
    const msg = error instanceof Error ? error.message : String(error);
    return { text: `Delete error: ${msg}`, isError: true };
  }
}

export async function handleDiff(
  fromArg?: string,
  toArg?: string,
): Promise<{ text: string; isError: boolean }> {
  if (!pluginState.manager || !pluginState.environmentReady) {
    return { text: UNAVAILABLE_MSG, isError: true };
  }
  if (!fromArg) {
    return {
      text: "Usage: ws-ckpt-diff <from> [<to>]\n  from: source snapshot id\n  to:   target snapshot id",
      isError: true,
    };
  }
  const result = await pluginState.manager.execDiffRaw(fromArg, toArg ?? "HEAD");
  return { text: result.text, isError: !result.success };
}

export async function handleStatus(): Promise<{ text: string; isError: boolean }> {
  if (!pluginState.manager || !pluginState.environmentReady) {
    return { text: UNAVAILABLE_MSG, isError: true };
  }
  const result = await pluginState.manager.getStatus();
  const statusText = result.success
    ? `${result.message}\n(This is the complete daemon status report.)`
    : result.message;
  return { text: statusText, isError: !result.success };
}

export async function handleConfig(
  action?: string,
  key?: string,
  value?: string,
): Promise<{ text: string; isError: boolean }> {
  if (!pluginState.resolvedConfig) {
    return { text: UNAVAILABLE_MSG, isError: true };
  }

  const act = (action ?? "view").toLowerCase();

  if (act === "view") {
    const cfg = pluginState.resolvedConfig;
    const lines: string[] = ["Current ws-ckpt configuration:\n"];
    lines.push(`  autoCheckpoint: ${cfg.autoCheckpoint}`);

    // Always query daemon via `--format json` (stable versioned wire shape;
    // text format isn't a contract). No module-level cache — render
    // directly from this call's parsed result. Aligns with hermes plugin
    // and removes by construction the cross-call RMW race the old cache
    // had. Daemon unreachable / parse-error are reported explicitly; we
    // NEVER fall back to a potentially-stale "last-known" value without
    // saying so, and NEVER silently degrade either to "disabled" (that
    // conflation was the original openclaw bug).
    const cmd = new CommandExecutor();
    const cfgResult = await cmd.config();
    if (cfgResult.exitCode !== 0) {
      lines.push(`  maxSnapshotsNum: (daemon unreachable)`);
      lines.push(`  maxSnapshotsDuration: (daemon unreachable)`);
      lines.push(`  workspace: ${cfg.workspace}`);
      lines.push(`\n(could not query daemon: ${cfgResult.stderr.trim() || "no output"})`);
      return { text: lines.join("\n"), isError: false };
    }
    const parsed = parseWorkspaceCleanupJson(cfgResult.stdout);
    switch (parsed.kind) {
      case "parse-error":
        lines.push(`  maxSnapshotsNum: (unknown — daemon response unparseable)`);
        lines.push(`  maxSnapshotsDuration: (unknown — daemon response unparseable)`);
        break;
      case "disabled":
        lines.push(`  maxSnapshotsNum: not set - auto-cleanup disabled`);
        lines.push(`  maxSnapshotsDuration: not set - auto-cleanup disabled`);
        break;
      case "count":
        lines.push(`  maxSnapshotsNum: ${parsed.num}`);
        lines.push(`  maxSnapshotsDuration: not set`);
        break;
      case "age":
        lines.push(`  maxSnapshotsNum: not set`);
        lines.push(`  maxSnapshotsDuration: ${parsed.duration}`);
        break;
    }
    lines.push(`  workspace: ${cfg.workspace}`);
    lines.push("\nNote: maxSnapshotsNum / maxSnapshotsDuration are this workspace's effective auto-cleanup policy (per-ws override on top of the daemon default).");
    if (parsed.kind === "parse-error") {
      lines.push(`\n(daemon response did not match expected schema: ${parsed.reason})`);
    }
    return { text: lines.join("\n"), isError: false };
  }

  if (act === "update" || act === "set") {
    if (!key) {
      return {
        text: "Usage: ws-ckpt-config update <key> <value>\n  Available keys: autoCheckpoint, maxSnapshotsNum, maxSnapshotsDuration, workspace",
        isError: true,
      };
    }

    // Workspace resolution + per-ws scoping live in `cmd.config()`: it falls
    // back to `pluginState.resolvedConfig.workspace`, refuses (exit 2) if
    // neither is set, and reports the chosen ws via `usedWorkspace`.

    if (key === "maxSnapshotsNum") {
      if (value === undefined) {
        return { text: "maxSnapshotsNum requires a value (positive integer, or \"unset\" to restore inherit-global)", isError: true };
      }

      // unset = restore default (delete policy.toml) so admin's later global toggle wins.
      if (value === "unset") {
        const cmd = new CommandExecutor();
        const result = await cmd.config(undefined, { reset: true });
        if (result.exitCode !== 0) {
          return { text: `Failed to reset workspace policy: ${result.stderr}`, isError: true };
        }
        return { text: `Cleared: maxSnapshotsNum unset — workspace ${result.usedWorkspace} now inherits global auto-cleanup.`, isError: false };
      }

      // --- set path ---
      // parseInt("20abc") returns 20 (silent truncation); reject anything that
      // isn't a clean positive-integer literal so we match hermes's int(value).
      const trimmed = value.trim();
      if (!/^[1-9]\d*$/.test(trimmed)) {
        return { text: "maxSnapshotsNum must be a positive integer", isError: true };
      }
      const num = Number(trimmed);
      const cmd = new CommandExecutor();
      const result = await cmd.config(undefined, { enableAutoCleanup: true, autoCleanupKeep: String(num) });
      if (result.exitCode !== 0) {
        return { text: `Failed to configure workspace: ${result.stderr}`, isError: true };
      }
      return { text: `Updated workspace policy for ${result.usedWorkspace}: maxSnapshotsNum = ${num} (auto-cleanup enabled, keep ${num})`, isError: false };
    }

    if (key === "maxSnapshotsDuration") {
      if (value === undefined) {
        return { text: "maxSnapshotsDuration requires a value (e.g. \"7d\", \"24h\", or \"unset\" to restore inherit-global)", isError: true };
      }

      // unset = restore default (delete policy.toml) so admin's later global toggle wins.
      if (value === "unset") {
        const cmd = new CommandExecutor();
        const result = await cmd.config(undefined, { reset: true });
        if (result.exitCode !== 0) {
          return { text: `Failed to reset workspace policy: ${result.stderr}`, isError: true };
        }
        return { text: `Cleared: maxSnapshotsDuration unset — workspace ${result.usedWorkspace} now inherits global auto-cleanup.`, isError: false };
      }

      // --- set path ---
      const cmd = new CommandExecutor();
      const result = await cmd.config(undefined, { enableAutoCleanup: true, autoCleanupKeep: value });
      if (result.exitCode !== 0) {
        return { text: `Failed to configure workspace: ${result.stderr}`, isError: true };
      }
      return { text: `Updated workspace policy for ${result.usedWorkspace}: maxSnapshotsDuration = ${value} (auto-cleanup enabled, keep ${value})`, isError: false };
    }

    if (key === "autoCheckpoint") {
      if (value === undefined) {
        return { text: 'autoCheckpoint requires a value: "true" or "false"', isError: true };
      }
      const normalized = value.trim().toLowerCase();
      // LLM tool callers and shell users emit a wide vocabulary; accept the
      // common bool aliases instead of failing silently for anyone who didn't
      // read stderr. Canonical form remains "true"/"false" in tool descriptions.
      let coerced: boolean;
      if (["true", "1", "yes", "on", "enabled"].includes(normalized)) {
        coerced = true;
      } else if (["false", "0", "no", "off", "disabled"].includes(normalized)) {
        coerced = false;
      } else {
        return { text: `autoCheckpoint must be "true" or "false" (got "${value}")`, isError: true };
      }
      if (coerced) {
        const ws = pluginState.resolvedConfig.workspace;
        if (ws) {
          const cwdCheck = cwdInsideWorkspace(ws);
          if (cwdCheck.inside) {
            return { text: cwdInsideWorkspaceReason(cwdCheck.cwd, ws), isError: true };
          }
        }
      }
      pluginState.resolvedConfig.autoCheckpoint = coerced;
      const persistHint = coerced
        ? `\n\nNote: This change is in-memory only and will reset on Gateway restart.\nTo persist, run:\n  openclaw config set plugins.entries.ws-ckpt.config.autoCheckpoint true --strict-json\n(This will cause a Gateway restart.)`
        : `\n\nNote: This change is in-memory only and will reset on Gateway restart.\nTo persist, run:\n  openclaw config set plugins.entries.ws-ckpt.config.autoCheckpoint false --strict-json\n(This will cause a Gateway restart.)`;
      return {
        text: `Config updated: autoCheckpoint = ${coerced}${persistHint}`,
        isError: false,
      };
    }

    if (key === "workspace") {
      if (!value) {
        return { text: "workspace requires a path value", isError: true };
      }
      pluginState.resolvedConfig.workspace = value;
      return {
        text: `Config updated: workspace = ${value} (in-memory, will reset on Gateway restart)`,
        isError: false,
      };
    }

    return {
      text: `Unknown config key: ${key}. Available: autoCheckpoint, maxSnapshotsNum, maxSnapshotsDuration, workspace`,
      isError: true,
    };
  }

  return {
    text: `Unknown action: ${act}. Use "view" or "update".`,
    isError: true,
  };
}
