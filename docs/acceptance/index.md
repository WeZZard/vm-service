# Index of acceptance criteria

## Table of Contents

- Agent
- Orchestrator
- Operator

Each scenario names the [test layer](../arch/test-layers.md) that runs it. Stories are in [user stories](../user-stories/index.md).

## Agent

[agent.acceptance](agent.acceptance)

1. AC-1: Many operations on one lease use one guest session (US19, TL-2).
2. AC-2: Guest operations over a guest session match the ssh link (US19, TL-2, TL-4).
3. AC-3: Transferred bytes arrive intact or the request fails (US19, TL-1, TL-2, TL-4).
4. AC-4: A guest session lost during an exec reports uncertain and never reruns it (US19, TL-1, TL-2).
5. AC-5: A guest session lost during a read is retried once (US19, TL-1, TL-2).
6. AC-6: A remote session resumes after a reset without running a call twice (US20, TL-1, TL-2).
7. AC-7: A repeated execution ID returns the first outcome (US20, TL-1, TL-2).
8. AC-8: A silent connection is detected and resumed (US20, TL-2, TL-3).
9. AC-9: A resume from a new address keeps the session (US20, TL-2, TL-3).
10. AC-10: An expired session leaves outcomes queryable (US20, TL-1, TL-2).
11. AC-11: The remote session passes an HTTPS-only proxy (US20, TL-3).
12. AC-19: A guest session that falls silent fails its request by the silence limit (US19, TL-1, TL-2, TL-4).

## Orchestrator

[orchestrator.acceptance](orchestrator.acceptance)

1. AC-12: Release cancels a command running over a guest session (US22, TL-2, TL-4).
2. AC-13: Release and destroy leave no guest session behind (US22, TL-2, TL-4).
3. AC-14: A daemon restart makes in-flight calls uncertain, never repeated (US22, TL-2).

## Operator

[operator.acceptance](operator.acceptance)

1. AC-15: Both channels pass one conformance suite on live VMs (US21, TL-4).
2. AC-16: The guest link is selected by configuration (US21, TL-2).
3. AC-17: Offline suites need no Tart, SSH or network (US21, TL-1, TL-2).
4. AC-18: Live tests leave the installed service alone (US21, TL-4).
