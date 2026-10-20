# Index of the CLI scene

## Table of Contents

- Components

## Components

The CLI is the executable `machctl` ([MG-1](../../arch/rename.md#mg-1-names)). It replaces `vmctl` in one coordinated switch, with no alias ([MG-3](../../arch/rename.md#mg-3-one-coordinated-switch)).

1. CL-1: [The machine list](list.md#cl-1-the-machine-list). One list of every machine from every instance, local and discovered.
2. CL-2: [Acquire by capability](list.md#cl-2-acquire-by-capability). The CLI picks a machine that has the required capabilities and acquires it from its instance.
3. CL-3: [Naming a machine](list.md#cl-3-naming-a-machine). Every lease command names its machine, and the CLI finds the instance that owns it.
