"""Summarize fresh-process medians/ranges; render diagnostics, not client claims."""
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

root = Path(sys.argv[1])
out = Path(sys.argv[2])
out.mkdir(parents=True, exist_ok=True)
assert (root / 'COMPLETE').exists()
groups = defaultdict(list)
for folder in sorted(root.glob('[0-9]*')):
    assert (folder / 'COMPLETE').exists()
    audit = json.loads((folder / 'audit.json').read_text())
    data = json.loads((folder / 'stdout.json').read_text())
    groups[(audit['scenario'], audit['placement'], audit['load'], audit['variant'])].append((audit, data))
summary = []
for (scenario, placement, load, variant), runs in sorted(groups.items()):
    assert len(runs) == 3
    row = {'scenario': scenario, 'placement': placement, 'load': load, 'variant': variant, 'processes': 3,
           'requests_min': min(d['requests'] for _, d in runs), 'requests_max': max(d['requests'] for _, d in runs),
           'rps_median': statistics.median(d['requests_per_second'] for _, d in runs),
           'background_completions_median': statistics.median(d['background_completions_including_warmup'] for _, d in runs)}
    for stage in ('initial_queue', 'helper_schedule', 'engine_call', 'answer_queue', 'total'):
        for stat in ('mean', 'p50', 'p99', 'max'):
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
plt.rcParams.update({'font.size': 10, 'axes.spines.top': False, 'axes.spines.right': False, 'svg.hashsalt': 'engine-actor-probe'})
variants = ['baseline05', 'control06', 'counters06', 'metrics06']
labels = ['0.5 off', '0.6 off', '0.6 counters', '0.6 full timing']
colors = ['#53657a', '#24806f', '#d09a26', '#ba5550']
lookup = {(r['scenario'], r['placement'], r['load'], r['variant']): r for r in summary}


def save(fig, name):
    fig.savefig(out / (name + '.png'), dpi=170, bbox_inches='tight')
    fig.savefig(out / (name + '.svg'), bbox_inches='tight', metadata={'Date': None})
    svg = out / (name + '.svg')
    svg.write_text('\n'.join(line.rstrip() for line in svg.read_text().splitlines()) + '\n')
    plt.close(fig)


fig, axes = plt.subplots(1, 2, figsize=(11, 4.6))
for ax, load in zip(axes, ['idle', 'busy']):
    x = np.arange(2)
    for v, label, color, offset in zip(variants, labels, colors, [-.3, -.1, .1, .3]):
        rows = [lookup['immediate', placement, load, v] for placement in ['async', 'thread']]
        y = [r['total_p99_ms_median'] for r in rows]
        errors = [[r['total_p99_ms_median'] - r['total_p99_ms_min'] for r in rows],
                  [r['total_p99_ms_max'] - r['total_p99_ms_median'] for r in rows]]
        ax.bar(x + offset, y, .18, yerr=errors, color=color, label=label, capsize=2)
    ax.set_xticks(x, ['Shared async', 'Dedicated thread'])
    ax.set_yscale('log')
    ax.set_ylabel('Total p99 latency (ms, logarithmic)')
    ax.set_title('Idle' if load == 'idle' else '16 background actors / 128 producers')
    ax.grid(axis='y', alpha=.15)
axes[0].legend(fontsize=9)
fig.suptitle('Immediate loopback FCU replies: median and range of 3 fresh processes')
fig.text(.5, -.01, '3 s per run; closed-loop request stream; mock on separate core. Not a beacon/client benchmark.', ha='center', fontsize=9)
fig.tight_layout()
save(fig, 'immediate-p99')

fig, axes = plt.subplots(1, 2, figsize=(11, 4.6))
stages = ['initial_queue', 'helper_schedule', 'engine_call', 'answer_queue']
stage_labels = ['Initial mailbox', 'Helper scheduling', 'Engine client call', 'Answer mailbox']
for ax, placement in zip(axes, ['async', 'thread']):
    bottoms = np.zeros(4)
    for stage, label, color in zip(stages, stage_labels, ['#56637e', '#d29b3e', '#58a89a', '#bd6676']):
        # Sum per-run means before taking their mean: bars reconstruct mean total.
        values = [statistics.mean(d['stages'][stage]['mean_ns'] / 1e6
                                 for _, d in groups['immediate', placement, 'busy', v]) for v in variants]
        ax.bar(labels, values, bottom=bottoms, label=label, color=color)
        bottoms += values
    ax.set_ylabel('Mean latency (ms; average over 3 processes)')
    ax.set_title('Shared async' if placement == 'async' else 'Dedicated thread')
    ax.tick_params(axis='x', rotation=20)
axes[0].legend(fontsize=9)
fig.suptitle('Where latency occurs under background actor load')
fig.tight_layout()
save(fig, 'busy-stages')

fig, axes = plt.subplots(1, 2, figsize=(11, 4.6))
for ax, scenario, load in zip(axes, ['delay20ms', 'retry503'], ['busy', 'idle']):
    rows = [lookup[scenario, 'thread', load, v] for v in variants]
    y = [r['total_mean_ms_median'] for r in rows]
    errors = [[r['total_mean_ms_median'] - r['total_mean_ms_min'] for r in rows],
              [r['total_mean_ms_max'] - r['total_mean_ms_median'] for r in rows]]
    ax.bar(labels, y, yerr=errors, color=colors, capsize=3)
    ax.set_ylabel('Mean total latency (ms)')
    ax.tick_params(axis='x', rotation=20)
    ax.set_title('20 ms mock delay + actor load' if scenario == 'delay20ms' else 'One injected HTTP 503 per call, idle')
fig.suptitle('Dedicated actor: delayed responses and the real retry ladder')
fig.text(.5, -.01, 'Median/range of 3 processes. Retry probes are mechanism checks with very few calls, not tail estimates.', ha='center', fontsize=9)
fig.tight_layout()
save(fig, 'delay-and-retry')
print(json.dumps({'fresh_processes': sum(len(v) for v in groups.values()), 'groups': len(groups), 'figures': 3}))
