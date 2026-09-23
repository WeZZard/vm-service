# Historical two-VM VNC visual experiment

## Withdrawn implementation

- **This report describes the rejected patched-Tart implementation.** Its runtime patch, service code, test adapter, and runner are not part of the current supported source. Tart patching is prohibited.
- The actual screenshots and vLLM comparisons remain valid historical observations, but they are not evidence that the replacement design, stock Tart, or the normal relay upgrade works.
- Statements below describe the experiment at the time it ran. Former file paths and commands are historical references, not current installation instructions. See the [revised design](design.md), [current verification status](capability-transparency-verification.md), and [replacement acceptance specification](vnc-acceptance.md).

## Historical result

- The user-requested two-VM visual test passed for two disposable Ubuntu guests on host macOS build `26A428` after correcting a headless Tart startup stall.
- The source guest rendered two randomized patterns. The viewer guest connected through real VNC, and screenshots were taken from the viewer guest's X11 root window, not from the source guest.
- An actual vLLM service running `GLM-5.3-Flash-EXL3` evaluated both screenshots. The evaluator was not given the expected codes, shapes, or colors. A separate comparison matched its observations against the source-generated manifests.
- Both phase numbers, both random codes, and all four shape/color positions in each phase matched. This establishes a live update through the tested VNC path, not merely successful process launch or TCP connection.
- An Ubuntu “System program problem detected” dialog remained visible in both screenshots. It did not obscure the challenged shapes or codes, and the evaluator reported it. The dialog's cause was not investigated or declared fixed.

## Actual path and scope

- The candidate service used a private state directory, a private copy-on-write Tart store, and port `16247`. The production daemon and original golden images were unchanged.
- The runtime was built from the updated private Tart patch and ad-hoc signed with the virtualization entitlement for this isolated test. Merely compiling its debug executable had not supplied that entitlement.
- The source lease was acquired through vm-service with `vnc: true` and `env: none`. The second guest was acquired without VNC and used as the viewer machine.
- A test-only launcher adapter received the password through the service's existing stdin interface. It connected an SSH remote loopback forward from the viewer guest to the service-owned proxy and started TigerVNC in that guest.
- The viewer password was converted with `tigervncpasswd` through stdin and held in an anonymous Linux memfd. It was not placed in argv, a URL, or an ordinary file.
- This exercised real private-runtime readiness, `console/open`, the service proxy, credential handoff, VNC authentication, framebuffer delivery, `console/cancel`, and lease release.
- This was **not** a live test of the macOS TurboVNC application or its native launcher. The configured executable was an isolated test adapter, and the actual display client was Linux TigerVNC. The production viewer implementation was not replaced.
- The test installed GUI test dependencies only into disposable purpose clones. No desktop window was opened on the physical host.

## Visual assertions

- Phase 1 displayed code `908A93`. Its quadrants were a lime square, an orange diamond, a yellow triangle, and a cyan circle, in reading order.
- Phase 2 displayed code `3765B2`. Its quadrants were a yellow circle, a lime triangle, a cyan square, and an orange diamond, in reading order.
- vLLM independently returned those values from the respective viewer screenshots. Exact structured comparison, with case normalization, passed for every expected field.
- The evaluator also identified the window title as `Virtualization - TigerVNC` and reported the system-problem dialog.
- The assistant separately inspected both actual images. No human visibility confirmation is claimed.

## Live defect found and corrected

- Initial private VNC acquisition timed out and rolled back. An ordinary no-VNC acquisition also failed to obtain an IP.
- A controlled comparison on the same owned diagnostic clone showed both stock Tart and the original patched build stalled in the nested AppKit run loop. The startup task did not execute.
- The updated patch awaits the MainActor startup task in the headless branch instead of entering `NSApplication.run()` from that actor. It retains the signal sources and leaves the graphical branch unchanged.
- The same clone booted and obtained an IP with the correction. Subsequent source and viewer acquisitions succeeded, and the visual assertions above used that corrected runtime.
- A windowless scheduling regression reproduces the old stall and the corrected task execution under both debug and optimized builds. Its first parent rerun exceeded the five-second outer timeout during cold initialization; the outer setup allowance was increased without weakening its watchdog or exit assertions. The final rerun passed both tests.
- The separate private protocol suite passed all 15 tests after the patch update. That suite has since been removed with the implementation.
- The parent/runtime readiness-budget mismatch identified during diagnosis remains a robustness concern for slower starts. The successful corrected boot does not prove that every supported startup completes within the current parent timeout.

## Listener, cancellation, and cleanup

- Read-only `lsof` inspection observed the source runtime's listener on `127.0.0.1`, not a wildcard address. The inspection output is retained locally.
- `console/cancel` reported the attempt cancelled and transport disconnected. The source guest then successfully executed `printf SOURCE_STILL_RUNNING`.
- A subsequent viewer-guest process check found no `vncviewer` process, and a post-disconnection viewer screenshot was retained.
- Both test leases were explicitly released. The candidate reported an empty lease map, and its Tart inventory contained only the stopped copied base. The diagnostic clone was also absent.
- The candidate daemon was stopped only after those checks. Its isolated stopped base and private test files remain under `/private/var/tmp/vnc-visual-69f51e63` for diagnosis; no VM remains running there.

## Evidence

- Local evidence is under `test-evidence/two-vm-vnc/`, which is ignored by Git.
- `viewer-phase1.png` and `viewer-phase2.png` are the actual viewer-guest screenshots. `viewer-disconnected.png` records the viewer guest after cancellation.
- `expected-phase1.json` and `expected-phase2.json` contain the source-generated expectations. `vllm-phase1.json` and `vllm-phase2.json` retain the raw model responses.
- `visual-assertions.json` contains the exact-match decisions and screenshot SHA-256 hashes. `listener.log`, `cancel.json`, and `cleanup.json` record the other bounded observations.
- The renderer and test adapter remain in the isolated private test directory. The vLLM requests used SSH to `spark-left` and its loopback OpenAI-compatible endpoint; no Anthropic gateway request was made.

## Remaining acceptance

- This result does not cover a macOS guest, the real host-side TurboVNC launcher, natural TTL expiry, a service restart with an active viewer, incorrect-password rejection, or agreement with mcp-vm-relay screenshots.
- The two-VM experiment was executed directly with isolated tooling and was not integrated into a single reusable automated runner. The separate `vnc_live.py` runner used human visual confirmation and has since been removed.
- Production activation and a claim that the complete mcp-vm-relay viewing workflow is delivered remain separate from this successful Linux-to-Linux VNC visual test.
