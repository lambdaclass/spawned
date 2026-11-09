#!/usr/bin/env python3
"""Render matched full-actor measurements; requires requirements-plots.txt."""
import argparse,base64,csv,json,os,statistics
from pathlib import Path
import matplotlib
matplotlib.use('Agg')
import matplotlib.pyplot as plt
import numpy as np
import markdown
VARIANTS=['async','actor-tokio-1','actor-tokio-16','actor-tokio-adaptive','actor-sharded-1','actor-sharded-16','actor-sharded-adaptive']
CASES=['active128_p128','active1024_p256','hotspot128','async_wait128','blocking128','tcp128']
LABELS=['128 actors / 128 producers','1024 actors / 256 producers','Skewed traffic, 128 actors','Async timer, 128 actors','Blocking handler, 128 actors','Persistent TCP, 128 actors']
COLORS=['#253746','#6baed6','#3182bd','#08519c','#a1d99b','#31a354','#006d2c']
def stats(rows,key):
 v=[float(r[key]) for r in rows if r['status']=='ok' and r.get(key) not in [None,'']]
 return (statistics.median(v),min(v),max(v)) if v else None

def main():
 p=argparse.ArgumentParser();p.add_argument('--data',type=Path,required=True);p.add_argument('--output',type=Path,required=True);a=p.parse_args()
 a.output.mkdir(parents=True,exist_ok=True);(a.output/'graphs').mkdir(exist_ok=True)
 studies={d.name:(json.loads((d/'metadata.json').read_text()),list(csv.DictReader((d/'raw.csv').open()))) for d in sorted(a.data.iterdir()) if (d/'raw.csv').exists()}
 plt.rcParams.update({'font.size':9,'axes.spines.top':False,'axes.spines.right':False,'axes.grid':True,'grid.alpha':.18,'figure.dpi':130,'svg.hashsalt':'spawned-full-actors'})
 allrows=[r for _,rows in studies.values() for r in rows]
 doc=['# Spawned: full actor sharding and batching','',f"{len(allrows):,} measured processes, {sum(r['status']=='ok' for r in allrows):,} successful; {sum(int(r.get('messages') or 0) for r in allrows if r['status']=='ok'):,} validated requests. Five independent processes per cell. Existing public actor/protocol interfaces and production defaults are unchanged.",'', 'Variant suffixes -1/-16 mean message-turn size, not actor or thread count. All compared paths run actual Spawned actors, boxed envelopes, unbounded mailboxes, replies and lifecycle. This study replaces the previous native-loop semantic mismatch with a matched comparison. It does not erase runtime ownership, placement and compatibility differences.','', '[Implementation and research questions](../../../docs/research/full-actor-shards.md) · [Validation and known test limitations](../../VALIDATION-FULL-ACTORS.md) · [Raw data and hashes](../../results/full-actors-2026-11-05/manifest.json)','']
 findings=Path(__file__).resolve().parent.parent/'legacy'/'benchmarks'/'FINDINGS-FULL-ACTORS.md'
 if findings.exists():doc += [findings.read_text().replace('(RESEARCH-NEXT.md)','(../../RESEARCH-NEXT.md)'),'']
 doc += ['## Reading the figures','','Bars show medians; whiskers show observed min/max across fresh processes, not confidence intervals. p99 is the median of run-level handler-completion p99s, not a pooled quantile. Caller-observed latency is retained separately in the CSV. Closed-loop traffic has at most one request per producer; these are not fixed-arrival-rate SLO tests. Compare within a platform and worker budget; The linux2/linux4 studies use a four-vCPU OrbStack VM/container; fede2/fede4 use native x86-64 Linux on a shared server with the entire process tree restricted to four physical CPUs. These are separate environments, not an isolated OS/kernel comparison.','']
 figs=[];summary=[]
 for study,(meta,rows) in studies.items():
  configs=[json.loads(r['config_json']) for r in rows];workers=configs[0]['workers']
  doc += [f'## {study}: {workers} actor workers','',f"Source `{meta['revision']}` (dirty: `{meta['dirty']}`), {meta['platform']}, {meta['rustc']}. File limits `{meta['nofile']}`; cgroup `{meta['cgroup']}`.",'', 'Sharded clients have a current-thread observer plus N actor workers; ordinary/batched Tokio uses N actor workers on the caller runtime. Producers and the common thread-per-socket echo server are additional equal controls. '+('The entire fede job used taskset CPUs 2–5 (four distinct physical cores in one L3 group), nice=10; actor workers were not individually pinned. Producer and TCP server threads share that CPU set. The host was shared, with unrelated activity and frequency scaling uncontrolled. [Host/topology metadata](../../results/full-actors-2026-11-05/fede-host.json).' if study.startswith('fede') else 'Affinity, frequency scaling and unrelated host activity are uncontrolled.')+'','']
  for metric,factor,unit,title,suffix in [('completed_per_s',1e-3,'k completed replies / second','Throughput','throughput'),('completion_p99_ns',1e-6,'Completion p99 (ms)','Tail latency','latency')]:
   fig,axes=plt.subplots(2,3,figsize=(17,9.5))
   for ax,case,label in zip(axes.flat,CASES,LABELS):
    for i,(variant,color) in enumerate(zip(VARIANTS,COLORS)):
     s=stats([r for r in rows if r['variant']==variant and r['case']==case],metric)
     if s:
      mid,lo,hi=(v*factor for v in s);ax.barh(i,mid,color=color,xerr=[[mid-lo],[hi-mid]],capsize=2,height=.68)
    ax.set_yticks(range(len(VARIANTS)),VARIANTS);ax.invert_yaxis();ax.set_title(label);ax.set_xlabel(unit);ax.set_xlim(left=0)
   fig.suptitle(f'{study}: {title} at {workers} actor workers',fontsize=14)
   fig.tight_layout(rect=(0,0,1,.97));name=f'{study}-{suffix}';png=a.output/'graphs'/f'{name}.png';svg=png.with_suffix('.svg')
   fig.savefig(png,bbox_inches='tight');fig.savefig(svg,metadata={'Date':None},bbox_inches='tight');plt.close(fig)
   svg.write_text('\n'.join(line.rstrip() for line in svg.read_text().splitlines())+'\n')
   figs.append(png);doc += [f'![{study} {title}](graphs/{png.name})','']
  failures=[r for r in rows if r['status']!='ok']
  if failures:doc += ['Failed samples (excluded from medians): '+', '.join(f"{r['variant']}/{r['case']}/{r['repeat']}: {r['status']}" for r in failures),'']
  doc += ['### Per-cell results','','| Variant | Case | n | Replies/s | Completion p99 ms | Cold p99 ms | CPU s | Peak RSS MB |','|---|---|---:|---:|---:|---:|---:|---:|']
  for case in sorted({r['case'] for r in rows}):
   for variant in VARIANTS:
    group=[r for r in rows if r['variant']==variant and r['case']==case]
    if not group:continue
    rec={'study':study,'workers':workers,'variant':variant,'case':case,'successful':sum(r['status']=='ok' for r in group),'samples':len(group)}
    for key in ['completed_per_s','completion_p99_ns','observed_p99_ns','queue_p99_ns','handler_p99_ns','enqueue_lag_p99_ns','hot_p99_ns','cold_p99_ns','rss_bytes','cpu_s','allocations','allocated_bytes']:
     values=stats(group,key)
     if values:
      for k,v in zip(['median','min','max'],values):rec[f'{key}_{k}']=v
    summary.append(rec)
    if rec['successful']:
     rate=f"{rec['completed_per_s_median']:,.0f}" if case!='idle4' else '— idle probe'
     cold=f"{rec['cold_p99_ns_median']/1e6:.4f}" if case=='hotspot128' else '—'
     doc += [f"| {variant} | {case} | {rec['successful']} | {rate} | {rec['completion_p99_ns_median']/1e6:.4f} | {cold} | {rec['cpu_s_median']:.2f} | {rec['rss_bytes_median']/1e6:.1f} |"]
  doc += ['','CPU/RSS include process startup, warmup, native producer/server threads, sample storage, sorting and teardown. They are not isolated actor-core utilization or per-actor heap. Allocation instrumentation is enabled only in alloc1; zero elsewhere means unavailable.','']
 doc += ['## Compatibility and interpretation','','The sharded backend is Tokio-based and statically places actors; it adds neither work stealing nor CPU affinity. Current-thread runtime restrictions apply, including unsupported block_in_place. The process pool and its explicit terminal shutdown are experimental. External Context helper calls retain their caller-runtime binding. Full actor tests do not make those runtime differences disappear.','', 'The initial actor-count-owned pool was rejected after agy review found premature helper/child runtime teardown. The corrected pool has explicit lifetime, and turn-budget/cancellation regressions accompany it. Keep default Tokio until both workload evidence and lifecycle ownership justify a supported alternative.','']
 report='\n'.join(doc);(a.output/'REPORT.md').write_text(report)
 fields=list(dict.fromkeys(k for rec in summary for k in rec))
 with (a.output/'SUMMARY.csv').open('w') as f:
  w=csv.DictWriter(f,fieldnames=fields,lineterminator='\n');w.writeheader();w.writerows(summary)
 body=markdown.markdown(report,extensions=['tables','fenced_code'])
 for png in figs:body=body.replace(f'src="graphs/{png.name}"',f'src="data:image/png;base64,{base64.b64encode(png.read_bytes()).decode()}"')
 page='''<!doctype html><html lang="en"><meta charset="utf-8"><meta name="viewport" content="width=device-width"><title>Spawned full actor study</title><style>body{max-width:1350px;margin:40px auto;padding:0 24px;background:#fcfcfa;color:#1e3040;font:16px/1.6 system-ui}h1,h2,h3{line-height:1.2}h2{margin-top:3em;border-bottom:2px solid #d6e0e6;padding-bottom:.4em}a{color:#176a86}img{width:100%;height:auto;background:white}table{display:block;overflow-x:auto;border-collapse:collapse;font-size:12px;width:100%;margin:1.5em 0}th,td{padding:7px 9px;border-bottom:1px solid #dce4e8;text-align:left}th{background:#e9f0f3}pre{overflow:auto;background:#eef2f3;padding:12px}@media print{h2{break-before:page}img{break-inside:avoid}}</style><body>'''+body+'</body></html>'
 (a.output/'REPORT.html').write_text(page);print('Rendered',len(figs),'figures and',len(summary),'cells')
if __name__=='__main__':main()
