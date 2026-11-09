"""Rebuild static figures/tables from preserved raw process outputs."""
import csv,json,pathlib,statistics
from collections import defaultdict
import matplotlib
matplotlib.use('Agg')
matplotlib.rcParams['svg.hashsalt'] = 'spawned-framework-metrics-2026-11-06'
import matplotlib.pyplot as plt
root=pathlib.Path(__file__).resolve().parent
groups=defaultdict(list)
for row in csv.DictReader((root/'data/raw.csv').open()):
 groups[(row['mode'],int(row['actors']),int(row['work']),row['profile'])].append(float(row['requests_per_second']))
cases=[('async',1,0),('async',128,0),('async',128,800),('threads',1,0)]
profiles=['baseline','off','counts','sample16','full']
labels=['Pre-metrics','Compiled out','Counters only','Timing 1/16','Full timing']
fig,axes=plt.subplots(2,2,figsize=(12,8),constrained_layout=True)
summary=[]
for ax,case in zip(axes.flat,cases):
 values=[groups[(*case,p)] for p in profiles]
 medians=[statistics.median(v) for v in values]
 errors=[[m-min(v) for m,v in zip(medians,values)],[max(v)-m for m,v in zip(medians,values)]]
 ax.bar(range(5),[m/1e6 for m in medians],yerr=[[e/1e6 for e in part] for part in errors],capsize=3,color=['#667788','#507cba','#41a69a','#b89b43','#af6e8e'])
 ax.set_xticks(range(5),labels,rotation=20,ha='right')
 ax.set_ylabel('Million completed requests / second')
 mode,actors,work=case
 ax.set_title(f'{mode}: {actors} actor(s), {actors} producer(s), {work} LCG steps')
 ax.grid(axis='y',alpha=.2)
 for p,m,v in zip(profiles,medians,values):
  summary.append({'mode':mode,'actors':actors,'work':work,'profile':p,'replicates':len(v),'median_requests_per_second':round(m,3),'min_requests_per_second':round(min(v),3),'max_requests_per_second':round(max(v),3),'relative_to_baseline':round(m/medians[0],6)})
fig.suptitle('Framework metrics overhead: 3 fresh processes per cell, 2 s each\nFour logical CPUs; ranges show min–max, not confidence intervals',fontsize=13)
fig.savefig(root/'throughput.png',dpi=170)
fig.savefig(root/'throughput.svg', metadata={'Date': None})
plt.close(fig)
with (root/'SUMMARY.csv').open('w') as f:
 w=csv.DictWriter(f,fieldnames=list(summary[0]),lineterminator="\n");w.writeheader();w.writerows(summary)
confirm=defaultdict(list)
for row in csv.DictReader((root/'data/confirmation.csv').open()):
 confirm[(row['mode'],int(row['actors']),int(row['work']),row['profile'])].append(float(row['requests_per_second']))
fig,ax=plt.subplots(figsize=(10,4.8),constrained_layout=True)
ratios=[]
for i,case in enumerate(cases):
 base=confirm[(*case,'baseline')];off=confirm[(*case,'off')]
 ratio=statistics.median(off)/statistics.median(base)
 # Conservative range of observed cross-pairs, not paired CI.
 low=min(off)/max(base);high=max(off)/min(base)
 ratios.append({'mode':case[0],'actors':case[1],'work':case[2],'baseline_median':round(statistics.median(base),3),'off_median':round(statistics.median(off),3),'median_ratio':round(ratio,6),'cross_pair_min_ratio':round(low,6),'cross_pair_max_ratio':round(high,6)})
 ax.bar(i,ratio,color='#507cba',yerr=[[ratio-low],[high-ratio]],capsize=5)
ax.axhline(1,color='black',linestyle='--',linewidth=1)
ax.set_xticks(range(4),[f'{mode}\na={actors}, work={work}' for mode,actors,work in cases])
ax.set_ylabel('Compiled-out throughput / pre-metrics throughput')
ax.set_title('Feature-off confirmation: 7 fresh processes per cell, 3 s each\nBars: ratio of medians; whiskers: observed cross-pair range (not a CI)')
ax.grid(axis='y',alpha=.2)
fig.savefig(root/'feature-off.png',dpi=170)
fig.savefig(root/'feature-off.svg', metadata={'Date': None})
plt.close(fig)
with (root/'CONFIRMATION.csv').open('w') as f:
 w=csv.DictWriter(f,fieldnames=list(ratios[0]),lineterminator="\n");w.writeheader();w.writerows(ratios)
print(json.dumps(ratios,indent=2))

# SVG path-data line endings contain cosmetic trailing spaces from Matplotlib.
for path in root.glob('*.svg'):
    path.write_text('\n'.join(line.rstrip() for line in path.read_text().splitlines())+'\n')
