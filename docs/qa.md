# QA

## 2026-03-09

### How should the built-in footer and `/statusline` relate?

The built-in idle footer is the richer layout:

- left: `project:branch +A/-D` when git data is available
- center: `provider • model • reasoning`, plus `• plan` in Plan mode
- right: context text such as `50% left`

`/statusline` remains a backward-compatible flat ordered override. It does not get left/center/right slots.

### What should `/statusline` expose?

Keep all existing item ids working and add additive items for:

- `project-name`
- `provider`
- `reasoning-effort`
- `mode`
- `git-changes`
- `git-summary`

The `/statusline` preview must show the real built-in footer when no override is set.
