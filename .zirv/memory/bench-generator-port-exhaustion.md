## Memory
- Key: bench-generator-port-exhaustion
- Written-by: claude
- Written: 1795740121
- Verified: 1795740121
- Source: explicit

Bench overload steps on this Windows host: 30 s transport errors in the loadgen reports (status.transport_errors, API p99 about 30,000 ms) come from the generator host running out of ephemeral ports, not from requests hanging in serve. Windows has 16,384 dynamic TCP ports (49152-65535). When serve answers slowly (about 9.5 s per request under pool saturation on 1.0.39), the two open-loop generators at about 1,900 requests/s hold about 18,000 connections. Evidence: System log provider Tcpip, events 4227 and 4231 ('all such ports being in use'), at the second the requests time out. This was seen in 1.0.38 c16 step 800 (2026-11-25 23:28:16 UTC) and in 1.0.39 c16 step 900 and c32 step 800 (docs/benchmarks/tier-l-1.0.39-rerun.md section 4). Check with: Get-WinEvent -FilterHashtable @{LogName='System'; ProviderName='Tcpip'; Id=4227,4231}.
