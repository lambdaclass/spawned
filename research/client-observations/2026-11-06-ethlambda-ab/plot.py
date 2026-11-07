#!/usr/bin/env python3
"""Plot archived read-only eth4/eth5 observations; requires matplotlib."""
import datetime as dt
import json
import pathlib
import statistics
import matplotlib
matplotlib.use('Agg')
import matplotlib.pyplot as plt
import matplotlib.dates as dates

root=pathlib.Path(__file__).resolve().parent
out=root/'figures';out.mkdir(exist_ok=True)
plt.rcParams.update({'font.size':10,'axes.spines.top':False,'axes.spines.right':False,'svg.hashsalt':'ethlambda-observations'})
colors=['#287ac0','#ef8a27']
def save(fig,name):
 fig.tight_layout()
 for ext in ['png','svg']:
  p=out/(name+'.'+ext);fig.savefig(p,dpi=160,bbox_inches='tight')
  if ext=='svg':p.write_text('\n'.join(line.rstrip() for line in p.read_text().splitlines())+'\n')
 plt.close(fig)
def result(name):return json.loads((root/'data'/name).read_text())['response']['data']['result']
fig,axes=plt.subplots(2,1,figsize=(11,6),sharex=True)
for ax,id,scale,label in [(axes[0],17,1000,'STF sections per import (ms)'),(axes[1],34,1,'Process CPU cores')]:
 for ref,color,node in [('A',colors[0],'eth4'),('B',colors[1],'eth5')]:
  values=result(f'series-{id}-{ref}.json')[0]['values']
  ax.plot([dt.datetime.fromtimestamp(t,dt.timezone.utc) for t,_ in values],[float(v)*scale for _,v in values],label=node,color=color)
 ax.set_ylabel(label);ax.grid(alpha=.2);ax.legend(loc='upper right')
 for timestamp in ['2026-11-06T20:21:56+00:00','2026-11-06T21:47:11+00:00','2026-11-06T22:11:32+00:00']:
  ax.axvline(dt.datetime.fromisoformat(timestamp),color='#777777',linestyle=':',alpha=.65)
axes[0].set_title('Production observations: eth5 changes from 0.5 to 0.6 at 20:21:56 UTC; later dotted lines are restarts')
axes[1].xaxis.set_major_formatter(dates.DateFormatter('%H:%M',tz=dt.timezone.utc));axes[1].set_xlabel('UTC, 6–7 November 2026 • 15-minute rolling rates • hardware/storage equivalence unverified')
save(fig,'warmup-and-cpu')

frames=json.loads((root/'data/paired-block-response.json').read_text())['results'];nodes={}
for ref,r in frames.items():
 f=r['frames'][0];names=[s['name'] for s in f['schema']['fields']]
 nodes[ref]={row['block_root']:row for row in [dict(zip(names,v)) for v in zip(*f['data']['values'])]}
keys=sorted(nodes['A'].keys()&nodes['B'].keys(),key=lambda k:nodes['A'][k]['received'])
assert len(keys)==150
fig,axes=plt.subplots(1,2,figsize=(12,4.5))
for ref,color,node in [('A',colors[0],'eth4'),('B',colors[1],'eth5')]:
 axes[0].plot(range(len(keys)),[nodes[ref][k]['total_ms'] for k in keys],label=node,color=color)
 axes[1].plot(range(len(keys)),[nodes[ref][k]['el_ms'] for k in keys],label=node,color=color)
for ax,title in zip(axes,['Received → attestable','Execution-client request']):
 ax.set_title(title);ax.set_xlabel('Same 150 blocks, ordered by eth4 receipt');ax.set_ylabel('Milliseconds');ax.grid(alpha=.2);ax.legend()
fig.suptitle('Matched blocks • 01:25:39–01:55:39 UTC • observational comparison, not a causal version benchmark')
save(fig,'matched-block-tails')

summary=json.loads((root/'data/paired-summary.json').read_text())['metrics']
fig,ax=plt.subplots(figsize=(9,4))
left=[0,0]
for field,label,color in [('before_ms','Before execution (column wait excluded)','#99bfd4'),('columns_wait_ms','Data-column wait','#e9bd64'),('exec_ms','STF / execution step','#82b583'),('head_ms','Post-STF to attestable (includes outstanding EL)','#c49abd')]:
 values=[summary[field][ref]['mean'] for ref in ['A','B']]
 ax.barh(['eth4','eth5'],values,left=left,label=label,color=color)
 left=[a+b for a,b in zip(left,values)]
ax.set_xlabel('Mean received → attestable milliseconds, same 150 blocks')
ax.set_title('Extra delay is concentrated in column wait and post-STF / execution-client completion')
ax.legend(loc='upper left',bbox_to_anchor=(1,1));ax.grid(axis='x',alpha=.2)
save(fig,'matched-mean-breakdown')
