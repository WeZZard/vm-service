# Historical documentation for the rejected implementation

- **Tart patching is prohibited.** The user rejected the implementation and requested documentation-only history. These documents are retained for traceability, not as permission or instructions to rebuild, reinstall, or reuse that implementation.
- The old implementation commits were removed from `main` through a documentation-only rewrite above baseline `478ba86`. Subsequent stock-Tart implementation work is separate new source; it does not restore the deleted patch.
- Historical commands, paths, API names, test counts, and status claims below describe withdrawn work. Refer to [the current design](../design.md) and [current verification status](../capability-transparency-verification.md) for the active proposal and its limitations.
- [The rejected console contract](rejected-private-vnc-console-contract.md) records the former private runtime and viewer API. It is not an available API.
- [The rejected runtime patch notes](rejected-tart-patch.md) record the framework-layout approach and startup workaround. The patch itself and its executable fixtures have been discarded.
- [The former verification report](rejected-private-vnc-verification.md) records tests of the withdrawn implementation. Those counts are not replacement acceptance.
- [The rejected human-assisted runner notes](rejected-human-vnc-runner.md) describe the removed runner, not an executable command in the current checkout.
- [The headless comparison](rejected-headless-comparison.md) records why the old test required a local runtime fix. The underlying stock-runtime compatibility issue remains to be solved without that patch.
- [The historical two-VM experiment](../two-vm-vnc-acceptance.md) retains the successful screenshots and their actual limitations. It does not establish that stock Tart or the installed relay workflow works.
- Local ignored screenshots and private diagnostic files were not published with this rewrite. Their survival on the test host is not a supported installation dependency.
