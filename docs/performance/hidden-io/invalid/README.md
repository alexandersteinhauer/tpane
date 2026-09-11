# Rejected thread-time unit assumption

`thread-unit-probe.json` contains the initial diagnostic unit probe, not valid per-thread CPU measurements. The helper provisionally treated `proc_threadinfo.pth_user_time/system_time` like `proc_taskinfo` Mach ticks. Its validation rejected the result: thread sum 183.106 s versus process 4.395 s over five seconds, a factor of approximately 125/3.

On this host the thread fields are already nanoseconds, unlike the process task counters. After correcting only that conversion, the sum of stable-thread deltas matches the independently converted process delta closely. Validated runs are outside this directory. The rejected probe is retained for transparency and must not be summarized as performance data.
