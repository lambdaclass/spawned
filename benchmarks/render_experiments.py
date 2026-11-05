#!/usr/bin/env python3
"""Render team reports and scientific charts from retained experiment samples."""
import argparse,base64,csv,json,os,statistics
from pathlib import Path
import matplotlib
matplotlib.use('Agg')
import matplotlib.pyplot as plt
import numpy as np
import markdown
SCALE=['active128_p32','active128_p128','active512_p128','active1024_p256']
PREFERRED=['async','threads','blocking','plain-tokio','dedicated-current','dedicated-shared','dedicated-managed','sharded-16','ready-16','ready-adaptive','local-ready-16']

def aggregate(rows,metric):
    values=[float(r[metric]) for r in rows if r.get('status')=='ok' and r.get(metric) not in ['',None] ]
    return (statistics.median(values),min(values),max(values)) if values else None

def main():
 p=argparse.ArgumentParser();p.add_argument('--data',type=Path,required=True);p.add_argument('--output',type=Path,required=True);args=p.parse_args()
 out=args.output;out.mkdir(parents=True,exist_ok=True);(out/'graphs').mkdir(exist_ok=True)
 studies={}
 for folder in sorted(args.data.iterdir()):
  if (folder/'raw.csv').exists():studies[folder.name]=list(csv.DictReader((folder/'raw.csv').open()))
 plt.rcParams.update({'font.family':'DejaVu Sans','font.size':10,'axes.spines.top':False,'axes.spines.right':False,'axes.grid':True,'grid.alpha':.2,'figure.dpi':140,'savefig.bbox':'tight','svg.hashsalt':'spawned-design-study'})
 doc=['# Spawned backend design study','', 'Measured runtime, transport, scheduler and I/O experiments. Read the methodology and compatibility limits before choosing a backend. Public actor/protocol APIs and production defaults are unchanged.','', '## How to read this report','', 'Each platform is analyzed separately. Linux is an OrbStack ARM64 VM/container, not bare metal. Lines and bars use medians; error bars show observed min/max across repeated processes, not confidence intervals. Failed starts and watchdogs are retained and excluded from performance medians. Prototype controls omit some actor lifecycle semantics, so raw speed alone is not a replacement criterion.','']
 all_rows=[r for rows in studies.values() for r in rows]
 doc += [f"{len(all_rows):,} measured processes; {sum(r['status']=='ok' for r in all_rows):,} completed; {sum(int(r.get('messages') or 0) for r in all_rows if r['status']=='ok'):,} validated requests/echoes. [Raw datasets and hashes]({os.path.relpath(args.data,out)}/manifest.json).",'']
 findings=Path(__file__).parent/'DESIGN-FINDINGS.md'
 if findings.exists():doc += [findings.read_text(),'']
 figures=[]
 def save(fig,name,caption):
  png=out/'graphs'/f'{name}.png';svg=out/'graphs'/f'{name}.svg'
  fig.savefig(png);fig.savefig(svg,metadata={'Date':None});plt.close(fig)
  svg.write_text('\n'.join(line.rstrip() for line in svg.read_text().splitlines())+'\n')
  doc.extend([f'![{caption}](graphs/{name}.png)','',caption,'']);figures.append(png)
 def series(rows,variants,cases,metric,factor,ylabel,title,name,log=False):
  fig,ax=plt.subplots(figsize=(11,5.4))
  for v in variants:
   points=[]
   for x,c in enumerate(cases):
    stat=aggregate([r for r in rows if r['variant']==v and r['case']==c],metric)
    if stat and stat[0]>0:points.append((x,*[n*factor for n in stat]))
   if points:
    x,y,lo,hi=np.array(points).T
    ax.errorbar(x,y,yerr=[y-lo,hi-y],marker='o',capsize=2,label=v,linewidth=1.4)
  ax.set_xticks(range(len(cases)),['128 / 32','128 / 128','512 / 128','1024 / 256'] if cases==SCALE else cases)
  ax.set_xlabel('Actors / producers (both vary)' if cases==SCALE else 'Connections')
  ax.set_ylabel(ylabel);ax.set_title(title)
  if log:ax.set_yscale('log')
  ax.legend(fontsize=8,loc='upper left',bbox_to_anchor=(1.01,1));fig.tight_layout()
  save(fig,name,title+' — failed combinations are absent; see the failure table.')
 for name,rows in studies.items():
  doc.extend([f'## {name}',''])
  meta=json.loads((args.data/name/'metadata.json').read_text())
  total=sum(int(r.get('messages') or 0) for r in rows if r['status']=='ok')
  ok=sum(r['status']=='ok' for r in rows)
  doc.extend([f"Source `{meta['revision']}`; {meta['platform']}; {meta['rustc']}. {ok}/{len(rows)} samples completed, {total:,} measured requests/exchanges.",'',f"Descriptor limits: `{meta.get('nofile')}`; requested child limit `{meta.get('requested_fd_limit')}`. Linux cgroup limits: `{meta.get('cgroup',{})}`.",''])
  failures=[r for r in rows if r['status']!='ok']
  if failures:
   counts={}
   for r in failures:counts[(r['variant'],r['case'],r['status'])]=counts.get((r['variant'],r['case'],r['status']),0)+1
   doc+=['### Failed combinations','','| Variant | Case | Status | Samples |','|---|---|---|---:|']
   doc += [f'| {v} | {c} | {status} | {n} |' for (v,c,status),n in counts.items()];doc+=['']
  messages=[r for r in rows if r['group']=='messages']
  ios=[r for r in rows if r['group']=='io']
  if messages:
   if any(r['case'] in SCALE for r in messages):
    series(messages,PREFERRED,SCALE,'completed_per_s',1,'Completed replies / second',f'{name}: scaling throughput',name+'-scaling-throughput')
    series(messages,PREFERRED,SCALE,'completion_p99_ns',1e-6,'Handler-completion p99 (ms)',f'{name}: scaling latency',name+'-scaling-latency',True)
   idle=[r for r in messages if r['case']=='idle4' and r['status']=='ok']
   if idle:
    labels=[];means=[]
    for v in ['async','threads','dedicated-current','dedicated-managed','native-block','native-hybrid','native-spin','ready-16','local-ready-16']:
     vals=[float(r['cpu_s'])/float(r['process_wall_s']) for r in idle if r['variant']==v and r.get('cpu_s')]
     if vals:labels.append(v);means.append(statistics.median(vals))
    fig,ax=plt.subplots(figsize=(10,4.8));ax.bar(labels,means,color='#287f8e');ax.tick_params(axis='x',rotation=35);ax.set_ylabel('Process CPU seconds / wall second (core equivalents)');ax.set_title(f'{name}: idle-dominated process CPU cost');ax.axhline(1,color='#999',linestyle='--');fig.tight_layout();save(fig,name+'-idle','Idle cost includes warmup/startup and one measured request; it is not a power measurement.')
   alloc=[r for r in messages if r['case']=='alloc1' and '-reply-' in r['variant']]
   if alloc:
    labels=[];vals=[]
    for v in sorted({r['variant'] for r in alloc}):
     ns=[float(r['allocations'])/int(r['messages']) for r in alloc if r['variant']==v and r['status']=='ok']
     if ns:labels.append(v);vals.append(statistics.median(ns))
    fig,ax=plt.subplots(figsize=(10,4.8));ax.bar(labels,vals,color='#4484aa');ax.tick_params(axis='x',rotation=35);ax.set_ylabel('Allocation/reallocation calls per measured request');ax.set_title(f'{name}: matched Probe reply transports');fig.tight_layout();save(fig,name+'-reply-allocations','Counts include benchmark-driver work; compared message shapes are matched within the Probe group.')
   hot=[r for r in messages if r['case']=='hotspot128']
   if hot:
    labels=[];cold=[];busy=[]
    for v in ['async','threads','sharded-1','sharded-16','ready-1','ready-16','ready-adaptive','local-ready-16']:
     c=aggregate([r for r in hot if r['variant']==v],'cold_p99_ns');h=aggregate([r for r in hot if r['variant']==v],'hot_p99_ns')
     if c and h and c[0]>0:labels.append(v);cold.append(c[0]/1e6);busy.append(h[0]/1e6)
    fig,ax=plt.subplots(figsize=(11,5));x=np.arange(len(labels));ax.bar(x-.2,cold,.4,label='Cold actors');ax.bar(x+.2,busy,.4,label='Hot actor');ax.set_xticks(x,labels,rotation=30,ha='right');ax.set_yscale('log');ax.set_ylabel('Completion p99 (ms)');ax.set_title(f'{name}: hotspot fairness and placement');ax.legend();fig.tight_layout();save(fig,name+'-fairness','Static placement puts the hot actor on one shard; this tests imbalance as well as batch policy.')
  if ios:
   variants=[v for v in ['std-io','tokio-io','monoio-legacy','monoio-uring','tokio-mux','monoio-legacy-mux','monoio-uring-mux','tokio-mux4','monoio-legacy-mux4','monoio-uring-mux4'] if any(r['variant']==v for r in ios)]
   series(ios,variants,['1','16','128'],'completed_per_s',1,'Completed echo exchanges / second',f'{name}: persistent TCP I/O throughput',name+'-io-throughput')
   series(ios,variants,['1','16','128'],'p99_ns',1e-3,'Echo p99 (µs)',f'{name}: persistent TCP I/O latency',name+'-io-latency',True)
  doc+=['### Median results','','| Variant | Case | Successful samples | Replies/s | p99 (ms) | Peak RSS (MB) |','|---|---|---:|---:|---:|---:|']
  keys=sorted({(r['variant'],r['case']) for r in rows})
  for v,c in keys:
   group=[r for r in rows if r['variant']==v and r['case']==c];rate=aggregate(group,'completed_per_s');lat=aggregate(group,'completion_p99_ns' if group[0]['group']=='messages' else 'p99_ns');rss=aggregate(group,'rss_bytes')
   if rate:
    rate_text=f'{rate[0]:,.0f}' if c!='idle4' else '— (idle probe)'
    lat_text=f'{lat[0]/1e6:.4f}' if lat else '—'
    rss_text=f'{rss[0]/1e6:.1f}' if rss else '—'
    doc.append(f'| {v} | {c} | {sum(r["status"]=="ok" for r in group)} | {rate_text} | {lat_text} | {rss_text} |')
  doc+=['']
 doc+=['## Incorporation criteria','','A backend is eligible for incorporation only after both performance and compatibility evidence support it. Keep async Tokio as the general-purpose reference. Treat native runnable queues as scheduler prototypes requiring full lifecycle integration. Per-core placement can help locality while worsening skewed-load tails. Spinning consumes CPU even without useful work. The reply experiment is a transport control: the normal async path already uses Tokio oneshots. io_uring results are Linux VM evidence and cannot rank macOS kqueue against Linux kernels directly.','', '## All implementations and tests','','The following guide defines every backend/control and test, including semantic differences and future integration work.','']
 guide=(Path(__file__).parent/'EXPERIMENTS.md').read_text()
 guide=guide.replace('(../docs/',f'({os.path.relpath(Path(__file__).resolve().parent.parent/"docs",out)}/')
 doc.append(guide.replace('# Backend design experiments: team guide','### Study guide',1))
 report='\n'.join(doc);(out/'REPORT.md').write_text(report)
 body=markdown.markdown(report,extensions=['tables','fenced_code','toc'])
 for path in figures:
  body=body.replace(f'src="graphs/{path.name}"',f'src="data:image/png;base64,{base64.b64encode(path.read_bytes()).decode()}"')
 page='''<!doctype html><html lang="en"><meta charset="utf-8"><meta name="viewport" content="width=device-width"><title>Spawned backend design study</title><style>body{max-width:1180px;margin:40px auto;padding:0 24px;color:#182733;background:#fcfcfa;font:16px/1.6 system-ui,sans-serif}h1,h2,h3{line-height:1.2}h2{margin-top:3em;border-bottom:2px solid #d3e2e8;padding-bottom:.4em}a{color:#126780}table{border-collapse:collapse;width:100%;font-size:13px;margin:1.5em 0;display:block;overflow-x:auto}th,td{padding:8px 10px;border-bottom:1px solid #dbe2e5;text-align:left}th{background:#e9f0f3}img{max-width:100%;height:auto;background:white;border:1px solid #dde5e7}pre{overflow:auto;background:#edf2f3;padding:16px}code{font-size:.9em}@media print{body{margin:0;max-width:none}h2{break-before:page}img,table{break-inside:avoid}}</style><body>'''+body+'</body></html>'
 (out/'REPORT.html').write_text(page)
 print('Rendered',len(figures),'charts to',out)
if __name__=='__main__':main()
