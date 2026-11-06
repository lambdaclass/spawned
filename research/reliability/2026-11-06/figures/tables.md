| Baseline case | Completions/s, median [min–max] | Completion p99 ms, median |
|---|---:|---:|
| burst-async-a128-p128 | 2,552,109 [2,223,490–2,567,736] | 12.914 |
| rpc-async-a512-p256 | 1,212,744 [1,208,451–1,224,068] | 0.525 |
| rpc-async-a8-p128 | 1,283,660 [1,273,752–1,285,574] | 1.031 |
| rpc-blocking-a8-p128 | 1,400,217 [1,373,317–1,401,137] | 0.510 |
| rpc-dedicated-a8-p128 | 1,386,677 [1,356,872–1,418,835] | 0.532 |
| rpc-native-a128-p128 | 1,152,613 [969,460–1,190,766] | 0.134 |
| rpc-threads-a8-p128 | 1,487,653 [1,477,055–1,491,232] | 0.148 |
| rpc-tokio-a8-p128 | 1,323,434 [1,307,345–1,332,274] | 0.918 |

| Graph profile | Root replies/s, median [min–max] | Observed p99 ms, median |
|---|---:|---:|
| graph-native-deque | 316,534 [316,430–320,830] | 4.288 |
| graph-sharded | 275,609 [271,882–276,603] | 1.826 |
| graph-tokio | 392,454 [383,375–392,967] | 3.762 |

| Offered-load case | Reply timeouts | Worst cold cohort observed p99 ms |
|---|---:|---:|
| offered-native-deque-100000 | 0 / 65536 | 2.340 |
| offered-native-deque-1000000 | 49521 / 65536 | 67.218 |
| offered-sharded-100000 | 48482 / 65536 | 0.332 |
| offered-sharded-1000000 | 51643 / 65536 | 59.227 |
| offered-tokio-100000 | 0 / 65536 | 7.435 |
| offered-tokio-1000000 | 49381 / 65536 | 70.682 |
