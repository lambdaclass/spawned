"""Keep pooled request tails and independent process summaries for sparse probes."""
from collections import defaultdict, Counter
import json
import math
from pathlib import Path
import statistics
import sys

import matplotlib
matplotlib.use('Agg')
import matplotlib.pyplot as plt
import numpy as np

root, out = map(Path, sys.argv[1:])
assert (root / 'COMPLETE').exists()
out.mkdir(parents=True, exist_ok=True)
groups = defaultdict(list)
for p in sorted(root.glob('*/audit.json')):
    a = json.loads(p.read_text())
    d = json.loads((p.parent / 'stdout.json').read_text())
    assert a['load'] == 'idle' and a['gap_ms'] == 50 and a['seconds'] == 10
    groups[a['variant']].append(d)
summary = {}
for variant, runs in groups.items():
    assert len(runs) == 6
    samples = [s for r in runs for s in r['samples_ns']]
    totals = sorted(s[4] / 1e6 for s in samples)
    stage_names = ['initial_queue', 'helper_schedule', 'engine_call', 'answer_queue']
    slow = [s for s in samples if s[4] >= 1_000_000]
    dominant = Counter(stage_names[max(range(4), key=lambda i: s[i])] for s in slow)
    summary[variant] = {'processes': len(runs), 'requests': len(samples),
        'mean_ms': statistics.mean(totals), 'p50_ms': totals[math.ceil(len(totals) * .5)-1],
        'p99_ms': totals[math.ceil(len(totals) * .99)-1], 'max_ms': max(totals),
        'over_1ms': len(slow), 'over_5ms': sum(t > 5 for t in totals),
        'dominant_stage_for_requests_at_least_1ms': dict(dominant),
        'process_mean_ms': [r['stages']['total']['mean_ns'] / 1e6 for r in runs],
        'process_p99_ms': [r['stages']['total']['p99_ns'] / 1e6 for r in runs]}
(out / 'summary.json').write_text(json.dumps(summary, indent=2) + '\n')
plt.rcParams.update({'font.size': 10, 'axes.spines.top': False, 'axes.spines.right': False, 'svg.hashsalt': 'sparse-followup'})
variants = ['control06', 'fixed06', 'metrics06', 'fixedmetrics06']
labels = ['Published / off', '1 worker / off', 'Published / full', '1 worker / full']
colors = ['#596b84', '#24816e', '#c88444', '#b65e6e']
fig, axes = plt.subplots(1, 2, figsize=(11, 4.7))
for v, label, color in zip(variants, labels, colors):
    values = sorted(s[4] / 1e6 for r in groups[v] for s in r['samples_ns'])
    tail = np.arange(len(values), 0, -1) / len(values)
    axes[0].step(values, tail, where='post', label=label, color=color)
axes[0].set_xscale('log')
axes[0].set_yscale('log')
axes[0].set_xlabel('Total latency (ms)')
axes[0].set_ylabel('Fraction at or above latency')
axes[0].legend(fontsize=8)
axes[0].set_title('Pooled request tails; 6 processes per variant')
for i, (v, color) in enumerate(zip(variants, colors)):
    values = summary[v]['process_p99_ms']
    axes[1].scatter(i + np.linspace(-.12, .12, len(values)), values, color=color)
    axes[1].plot([i-.2, i+.2], [statistics.median(values)]*2, color=color, linewidth=2)
axes[1].set_xticks(range(4), labels, rotation=20)
axes[1].set_ylabel('Process p99 latency (ms)')
axes[1].set_title('Each point is one fresh 10-second process')
fig.suptitle('Idle dedicated actor: 50 ms gaps, published vs one-worker runtime')
fig.tight_layout()
fig.savefig(out / 'sparse-tails.png', dpi=170, bbox_inches='tight')
svg = out / 'sparse-tails.svg'
fig.savefig(svg, bbox_inches='tight', metadata={'Date': None})
svg.write_text('\n'.join(line.rstrip() for line in svg.read_text().splitlines()) + '\n')
plt.close(fig)
print(json.dumps(summary, indent=2))
