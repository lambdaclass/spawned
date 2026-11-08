"""Summarize the bounded-worker and concurrent/sparse follow-up campaigns."""
from collections import defaultdict
import csv
import json
from pathlib import Path
import statistics
import sys

import matplotlib
matplotlib.use('Agg')
import matplotlib.pyplot as plt
import numpy as np

base, out = map(Path, sys.argv[1:])
out.mkdir(parents=True, exist_ok=True)
groups = defaultdict(list)
for campaign, folder in [('one-worker', 'engine-one-worker-probe'), ('concurrent', 'engine-concurrent-probe')]:
    root = base / folder / 'results'
    assert (root / 'COMPLETE').exists(), campaign
    for run in sorted(root.glob('[0-9]*')):
        assert (run / 'COMPLETE').exists()
        audit = json.loads((run / 'audit.json').read_text())
        data = json.loads((run / 'stdout.json').read_text())
        key = (campaign, audit['scenario'], audit['placement'], audit['load'], audit.get('concurrency', 1), audit.get('gap_ms', 0), audit['variant'])
        groups[key].append((audit, data))
summary = []
for key, runs in sorted(groups.items()):
    assert len(runs) == 3
    row = dict(zip(['campaign', 'scenario', 'placement', 'load', 'concurrency', 'gap_ms', 'variant'], key))
    row.update(processes=3, requests_min=min(d['requests'] for _, d in runs), requests_max=max(d['requests'] for _, d in runs),
        rps_median=statistics.median(d['requests_per_second'] for _, d in runs),
        sampled_peak_threads_max=max(a['sampled_peak_threads'] for a, _ in runs),
        sampled_peak_rss_bytes_max=max(a['sampled_peak_rss_bytes'] for a, _ in runs))
    for stage in ['initial_queue', 'helper_schedule', 'engine_call', 'answer_queue', 'total']:
        for stat in ['mean', 'p50', 'p99', 'max']:
            values = [d['stages'][stage][stat + '_ns'] / 1e6 for _, d in runs]
            row[f'{stage}_{stat}_ms_median'] = statistics.median(values)
            row[f'{stage}_{stat}_ms_min'] = min(values)
            row[f'{stage}_{stat}_ms_max'] = max(values)
    summary.append(row)
(out / 'summary.json').write_text(json.dumps(summary, indent=2) + '\n')
with (out / 'summary.csv').open('w') as f:
    writer = csv.DictWriter(f, fieldnames=list(summary[0]), lineterminator='\n')
    writer.writeheader()
    writer.writerows(summary)
lookup = {tuple(r[k] for k in ['campaign', 'scenario', 'placement', 'load', 'concurrency', 'gap_ms', 'variant']): r for r in summary}
variants = ['control06', 'fixed06', 'metrics06', 'fixedmetrics06']
labels = ['Published / off', '1 worker / off', 'Published / full', '1 worker / full']
colors = ['#596b84', '#24816e', '#c88444', '#b65e6e']
plt.rcParams.update({'font.size': 9, 'axes.spines.top': False, 'axes.spines.right': False, 'svg.hashsalt': 'worker-followups'})


def bars(ax, rows_for_variant, stat):
    x = np.arange(len(rows_for_variant[0]))
    for rows, label, color, offset in zip(rows_for_variant, labels, colors, [-.3, -.1, .1, .3]):
        y = [r[stat + '_median'] for r in rows]
        errors = [[r[stat + '_median'] - r[stat + '_min'] for r in rows], [r[stat + '_max'] - r[stat + '_median'] for r in rows]]
        ax.bar(x + offset, y, .18, color=color, label=label, yerr=errors, capsize=2)
    ax.grid(axis='y', alpha=.15)


def save(fig, name):
    fig.tight_layout()
    fig.savefig(out / (name + '.png'), dpi=170, bbox_inches='tight')
    svg = out / (name + '.svg')
    fig.savefig(svg, bbox_inches='tight', metadata={'Date': None})
    svg.write_text('\n'.join(line.rstrip() for line in svg.read_text().splitlines()) + '\n')
    plt.close(fig)


fig, axes = plt.subplots(1, 2, figsize=(11, 4.4))
for ax, load in zip(axes, ['idle', 'busy']):
    rows = [[lookup['one-worker', 'immediate', placement, load, 1, 0, v] for placement in ['async', 'thread']] for v in variants]
    bars(ax, rows, 'total_p99_ms')
    ax.set_xticks([0, 1], ['Shared async', 'Dedicated actor'])
    ax.set_yscale('log')
    ax.set_ylabel('Total p99 (ms, logarithmic)')
    ax.set_title('Idle' if load == 'idle' else '16 background actors / 128 producers')
axes[0].legend(fontsize=8)
fig.suptitle('Published vs one-worker runtime: median / range of 3 fresh processes')
save(fig, 'one-worker-p99')

fig, axes = plt.subplots(1, 3, figsize=(15, 4.4))
for ax, scenario in zip(axes[:2], ['immediate', 'delay20ms']):
    rows = [[lookup['concurrent', scenario, 'thread', 'busy', c, 0, v] for c in [4, 16]] for v in variants]
    bars(ax, rows, 'total_p99_ms')
    ax.set_xticks([0, 1], ['4 in flight', '16 in flight'])
    ax.set_ylabel('Total p99 (ms)')
    ax.set_title('Immediate mock' if scenario == 'immediate' else '20 ms mock delay')
rows = [[lookup['concurrent', 'immediate', 'thread', load, 1, 50, v] for load in ['idle', 'busy']] for v in variants]
bars(axes[2], rows, 'total_mean_ms')
axes[2].set_xticks([0, 1], ['Idle', 'Busy'])
axes[2].set_ylabel('Mean total latency (ms)')
axes[2].set_title('50 ms gaps between requests')
axes[0].legend(fontsize=8)
fig.suptitle('Dedicated actors: concurrent RPCs and wakeups after idle gaps')
save(fig, 'concurrent-and-sparse')
print(json.dumps({'runs': sum(len(r) for r in groups.values()), 'groups': len(summary),
    'scored_requests': sum(d['requests'] for runs in groups.values() for _, d in runs)}))
