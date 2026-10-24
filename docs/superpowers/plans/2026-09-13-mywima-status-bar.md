# Mywima Status Bar Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add a bottom status bar to mywima with a terminal button and live clock.

**Architecture:** Render a 30px high bar at the bottom of the screen (`y: 770..800`) in `mywima/src/backends/kms.rs`, add hit-testing for the terminal button on mouse click, and format the time.

**Tech Stack:** Rust, Smithay, OpenGL / GLES.

---

### Task 1: Implement Status Bar Rendering and Click Handling in mywima

**Files:**
- Modify: `mywima/src/backends/kms.rs`

- [ ] **Step 1: Add status bar render logic**
Add a render element or colored box for the status bar at `y: 770..800` and handle left-click detection in the pointer event handler to spawn `/bin/foot`.

- [ ] **Step 2: Build and verify**
Build mywima and run filesystem build.

- [ ] **Step 3: Commit**
```bash
git add mywima/src/backends/kms.rs
git commit -m "feat(mywima): add bottom status bar with terminal button and clock"
```
