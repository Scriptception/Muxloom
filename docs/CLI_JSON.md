# Stable CLI JSON schema

`muxloom list --json` is the supported scripting surface. It emits a JSON array of workspace objects. Additive fields may appear in compatible releases; existing fields will not change meaning within the 1.x series.

Each workspace contains `id`, `name`, `created_at`, `panes`, `layout`, `respawn`, and optional `schedule_id`. Each pane contains `id`, `workspace_id`, `title`, `cwd`, argv-array `command`, `provider`, optional `pid`, `state`, optional `progress`, `started_at`, optional `exited_at`, optional `exit_status`, `daemon_lost`, and `unread`.

Consumers should ignore unknown fields and treat IDs as opaque strings. Dates are RFC 3339 UTC timestamps. `exited_at: null` plus `daemon_lost: true` means the daemon lost ownership without observing an exit; it does not mean the process is attachable.

`muxloom attention list --json` emits an array of attention events with `id`, `pane_id`, `provider`, `kind`, `severity`, `summary`, `created_at`, optional `read_at`, and `confidence`. `confidence` distinguishes `native`, `hook_derived`, and `output_inferred`; the legacy `inferred` value remains decodable for protocol compatibility.

`muxloom schedule list --json` emits schedule records with `id`, `name`, `expression`, `timezone`, `cwd`, argv-array `command`, `enabled`, optional `last_run_at`, and optional `next_run_at`.
