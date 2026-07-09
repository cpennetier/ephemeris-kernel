"""Ephemeris Kernel — a deterministic time-and-truth runtime.

Answers one question, exactly and reproducibly: at time t, what is the state
of the world, what events caused it, and can you prove it?

Main export: the compiled engine module.

    from ephemeris import _engine
    eng = _engine.PyEngine(node_ipc, lane_ipc, schedule_ipc, demand_ipc, ...)

The kernel contains no optimizer, no policy, and no domain. Consumers impose
decisions through the public hooks (forced routes, schedule operations,
injection, outages); the kernel executes, records, and replays them under a
total event order with cryptographic run manifests.
"""

__version__ = "0.1.0"
