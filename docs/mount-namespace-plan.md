# Mount Namespace Planning — Linux-compatible direction

## Goal
Split the global `MOUNTS` list into namespaces (`CLONE_NEWNS`) and
give each process (or task) its own `MountNamespace` chain.

## Architecture
1. Add `MountNamespace { lock: SpinLock<Vec<Arc<Mount>>>, parent: Option<Arc<MountNamespace>>, propagation: PropType }`.
2. `Process` holds `ns: Arc<MountNamespace>`.
3. Path walk resolves through `task().process.ns.mounts` rather than `MOUNTS`.
4. `mount(2)` can choose copy-on-write tree from parent.
5. `umount2` sees only namespace entries.
6. `pivot_root` replaces root entry in local tree.

## Lock ordering
No new lock if we reuse the existing `MOUNTS` table first (global)
as a bootstrap namespace; later drop it for per-process roots.

## Integration invariants
- `AGENT.md §1`: keep `process.lock()` innermost.
- `AGENT.md §16.5`: copy on `fork_address_space` but also share immutable KSM/KVFS.
- Regression path: standard `pivot_root` introduced later still respects `/proc/self/mountinfo`.

## Status
Design-only until build phases and more userspace namespace APIs appear. EOF
EOF
echo "done"