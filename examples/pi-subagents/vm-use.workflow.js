/**
 * vm-use workflow — run one bounded VM task through the vm-service.
 *
 * Usage (from pi):  subagent { workflow: "vm-use", args: { task: "..." } }
 * Or copy this script inline. The child agent:
 *   1. acquires a fresh VM via vmctl (TTL-leased, default gateway credential pack)
 *   2. does the task inside the guest
 *   3. transfers any evidence/outputs to the host (vmctl pull)
 *   4. ALWAYS releases the VM (finally block), even on failure
 *
 * Contract with the child: the task prompt instructs it to report
 * structuredOutput { verdict: 'done'|'blocked', summary, artifacts }.
 */
// args: { task: string, image?: 'macos26'|'ubuntu2404', ttlHours?: number, purpose?: string }

const purpose = (args.purpose || (args.task || 'task')
  .toLowerCase().replace(/[^a-z0-9]+/g, '-').replace(/^-+|-+$/g, '').slice(0, 40) || 'task')
  .slice(0, 63);
const image = args.image || 'macos26';
const ttl = args.ttlHours || 4;

const VMCTL = `${process.env.HOME}/Artifacts/Repositories/com.github/WeZZard/vm-service/bin/vmctl`;

// 1. acquire
const acquire = await runs.host('acquire', {
  kind: 'command',
  command: `${VMCTL} acquire --purpose ${purpose} --image ${image} --ttl-hours ${ttl} --json-output 2>/dev/null || ${VMCTL} acquire --purpose ${purpose} --image ${image} --ttl-hours ${ttl}`,
  timeoutMs: 20 * 60 * 1000,  // clone + boot can take several minutes
});
if (acquire.exitCode !== 0) {
  return { error: 'acquire failed', detail: acquire.output };
}
let vm = null;
try {
  const parsed = JSON.parse(acquire.output.slice(acquire.output.indexOf('{')));
  vm = parsed.vm;
} catch (e) {
  return { error: 'could not parse acquire output', detail: acquire.output };
}

// 2-4. work, then always release
try {
  const child = await runs.run('worker', {
    agent: 'claude',           // or your preferred executor agent
    task: [
      `You are working inside a leased macOS VM named ${vm}.`,
      `Host-side control (run via bash):`,
      `  ${VMCTL} exec ${vm} -- <command...>    # run a command in the guest`,
      `  ${VMCTL} exec ${vm} --script <host-file>`,
      `  ${VMCTL} push ${vm} <host-path> <guest-path>`,
      `  ${VMCTL} pull ${vm} <guest-path> <host-path>   # transfer evidence OUT before release`,
      ``,
      `TASK: ${args.task}`,
      ``,
      `Rules: do all VM-side work through vmctl exec/push/pull. Pull every artifact,`,
      `log, and evidence file to a host path under /var/tmp/ before finishing.`,
      `Do not release the VM yourself — the workflow owns teardown.`,
    ].join('\n'),
    output: `/var/tmp/vm-service-${vm}/worker-output.md`,
  });
  return { vm, childOutput: child.outputReference || child.output };
} finally {
  // 4. teardown is part of the job
  await runs.host('release', {
    kind: 'command',
    command: `${VMCTL} release ${vm} || true`,
    timeoutMs: 10 * 60 * 1000,
  });
}
