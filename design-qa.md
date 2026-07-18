**Comparison Target**

- Source visual truth: `/home/myo/.codex/generated_images/019f7333-2b07-7833-8482-7b297eb72cba/exec-1df1096f-60fe-408f-9ae2-49891ebe4b81.png`
- Normalized source: `artifacts/nivalis-reference-1200x900.png`
- Implementation screenshot: `artifacts/nivalis-mail-memory-optimized.png`
- Viewport: 1200 x 900 at 1x scale
- State: light theme, All inboxes selected, Maya Chen message selected, account warning visible, no transient snackbar

**Findings**

- No actionable P0, P1, or P2 mismatch remains.
- Fonts and typography: system Noto Sans and the compact size/weight hierarchy preserve the source's dense desktop rhythm. Small metadata remains readable and truncates instead of changing column geometry.
- Spacing and layout rhythm: the 40 px title bar, 248 px navigation pane, approximately 408 px message list, flexible reader, compact row height, toolbar grouping, radii, and outline hierarchy align with the source composition.
- Colors and visual tokens: neutral gray surfaces, thin borders, restrained blue selection, amber account warning, and red destructive actions match the source's low-elevation desktop language with clear contrast.
- Image and icon fidelity: the generated Nivalis mark is a real transparent raster asset sized for its title-bar slot. Material Symbols Rounded supplies consistent geometric interface icons; no visible asset is approximated with emoji, ASCII art, or handcrafted vector shapes.
- Copy and content: realistic mailbox content preserves the source hierarchy while reflecting the prototype's actual ten-message data set and account state.

**Full-View Evidence**

- Final side-by-side comparison: `artifacts/nivalis-qa-comparison-memory.png`
- The source and implementation use the same 1200 x 900 frame and default inbox-reading state. Major-region proportions, navigation placement, list density, reader hierarchy, and above-the-fold content align.

**Focused Evidence**

- Reader and toolbar comparison: `artifacts/nivalis-qa-reader.png`
- Navigation and message-list comparison: `artifacts/nivalis-qa-list.png`
- Compact message list: `artifacts/nivalis-mail-compact.png` at 720 x 760.
- Compact single-pane reader: `artifacts/nivalis-mail-compact-reader.png` at 720 x 760.
- These focused comparisons confirm icon/label alignment, message-header rhythm, attachment treatment, row density, selected state, warning surface, typography, and truncation behavior.

**Comparison History**

1. Initial comparison: `artifacts/nivalis-qa-comparison-1.png`, implementation `artifacts/nivalis-iteration-1.png`.
   Earlier P2 findings: title-bar controls and spacing did not match the source; message-list density and sample content were too sparse; the reader action bar and body rhythm materially differed.
   Fixes: rebuilt the frameless title bar, expanded realistic demo rows, tightened list geometry, changed reader actions to icon-over-label groups, and recalibrated header/body/attachment spacing.
2. Revised implementation: `artifacts/nivalis-iteration-2.png`.
   Post-fix evidence showed the major proportions and information hierarchy aligned. Remaining P3 polish was limited to reader toolbar spacing, body measure, and attachment width.
   Fixes: refined the reader toolbar grouping, text line length, metadata placement, and attachment geometry.
3. Final comparison: `artifacts/nivalis-qa-comparison.png` with focused evidence listed above.
   Result: no actionable P0/P1/P2 difference remains.
4. Compact interaction check found a P1 navigation defect: selecting a message updated the Rust model before setting `detail-open`, so virtual-list delegate replacement could discard the remaining UI assignment.
   Fix: set compact navigation state before invoking the synchronous Rust model update.
   Post-fix evidence: `artifacts/nivalis-mail-compact-reader.png` shows the selected message in the full-width reader with a visible Back control; no P0/P1/P2 issue remains.
5. Memory optimization changed the default surface from Skia OpenGL to Skia software rendering and split summary/detail models.
   Visual regression evidence: `artifacts/nivalis-memory-visual-comparison.png` compares the pre-optimization GPU capture with the optimized default capture; normalized pixel RMSE is 0.00132 and no layout, wrapping, icon, color, or interaction hierarchy regression is visible.
   Final source comparison: `artifacts/nivalis-qa-comparison-memory.png`; no P0/P1/P2 issue remains.

**Primary Interactions Tested**

- `Ctrl+N` opens the composer.
- The composer automatically focuses the recipient field; typed text enters that field.
- `Escape` closes the composer and returns to the inbox.
- At 720 x 760, selecting a message transitions from the compact list to the full-width reader.
- Window minimize, maximize/restore, close, and drag callbacks are wired to Slint's native window API.
- Store tests cover search, account/folder filtering, selection, read/star/archive/delete/undo, compose/send, sync, and loading/error state transitions.
- This is a native Slint application, so browser console checks do not apply. The native process remained responsive during capture and keyboard testing.

**Release Measurements**

- Stripped recommended release executable: 18.0 MB (`opt-level = "s"`).
- Embedded Material Symbols subset: 110 KB.
- Default Skia software profile, three fresh X11 runs at 1200 x 900: worst stable sample 35.5 MiB RSS / 21.2 MiB PSS / 18.0 MiB USS.
- Three native Wayland runs: worst stable sample 41.5 MiB RSS / 22.4 MiB PSS / 17.5 MiB USS.
- The opt-in Skia OpenGL profile remains available through `NIVALIS_RENDERER=skia`. Full methodology and growth results are in `memory-report.md`.

**Open Questions**

- None blocking. The source shows a transient undo snackbar and 214-message production-like totals; the implementation intentionally shows no fake default snackbar and reports its real ten-message demo model.

**Follow-up Polish**

- [P3] Add a real `Ctrl+K` search shortcut before displaying the source's shortcut hint.
- [P3] Back the bounded 50-summary model with SQLite plus IMAP/JMAP and load attachments from disk on demand.

**Implementation Checklist**

- [x] Match the selected visual direction and desktop density.
- [x] Preserve a single high-emphasis Compose action.
- [x] Implement adaptive navigation and reading layouts.
- [x] Include loading, empty, error, confirmation, and undo states.
- [x] Verify core keyboard flow and accessible focus behavior.
- [x] Compare full and focused regions against the source at the same viewport.

final result: passed
