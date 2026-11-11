#!/usr/bin/env python3
"""Render graph, offered-load and qualitative profile evidence from fede_research.py."""
import argparse
import base64
import csv
from collections import defaultdict
import json
from pathlib import Path
import statistics
import matplotlib
matplotlib.use('Agg')
import matplotlib.pyplot as plt
import numpy as np
import markdown

VARIANTS=['async','tokio-1','tokio-16','tokio-adaptive','sharded-1','sharded-16','sharded-adaptive']
COLORS=['#253746','#6baed6','#3182bd','#08519c','#a1d99b','#31a354','#006d2c']
METRICS=['replies_per_s','completion_p99_ns','observed_p99_ns','cold_p99_ns','hot_p99_ns',
         'cold_other_observed_p99_ns','cold_shard0_observed_p99_ns','worst_cold_actor_observed_p99_ns',
         'lag_p99_ns','observer_lag_p99_ns','timeouts','cold_timeouts','hot_timeouts','cold_late_observed',
         'hot_late_observed','cold_samples','hot_samples','peak_queued','peak_outstanding',
         'peak_hot_queued','peak_cold_queued','peak_rss_bytes','rss_bytes','cpu_s',
         'window_service_per_s','window_queue_growth_per_s','submit_elapsed_s','elapsed_s',
         'actor_cpu_ns_per_root','actor_cpu_ns_per_handler','actor_voluntary_switches_per_root','actor_runqueue_wait_ns_per_root']


def read_csv(path):
    with path.open() as file:
        return list(csv.DictReader(file))


def number(row,key,default=0):
    return float(row[key]) if row.get(key) not in [None,''] else default


def stats(rows,key,factor=1):
    values=[number(r,key)*factor for r in rows if r.get(key) not in [None,'']]
    return (statistics.median(values),min(values),max(values)) if values else None


def valid(row):
    return row.get('driver_valid') in [True,'True','true','1']


def main():
    parser=argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--data',type=Path,required=True)
    parser.add_argument('--output',type=Path,required=True)
    args=parser.parse_args()
    out=args.output;out.mkdir(parents=True,exist_ok=True);(out/'graphs').mkdir(exist_ok=True)
    rows=read_csv(args.data/'raw.csv');meta=json.loads((args.data/'metadata.json').read_text())
    for row in rows:
        row['config']=json.loads(row['config_json'])
    good=[r for r in rows if r['status']=='ok']
    for row in good:
        path=args.data/'threads'/f'{row["stem"]}.csv'
        if path.exists():
            threads=read_csv(path)
            runtime=sum(number(t,'runtime_ns') for t in threads)
            if runtime:
                row['actor_cpu_ns_per_root']=runtime/number(row,'messages')
                row['actor_cpu_ns_per_handler']=runtime/number(row,'logical_handlers')
                row['actor_voluntary_switches_per_root']=sum(number(t,'voluntary_switches') for t in threads)/number(row,'messages')
                wait=sum(number(t,'runqueue_wait_ns') for t in threads)
                if wait:row['actor_runqueue_wait_ns_per_root']=wait/number(row,'messages')
    plt.rcParams.update({'font.size':9,'axes.spines.top':False,'axes.spines.right':False,
                         'axes.grid':True,'grid.alpha':.2,'figure.dpi':140,'svg.hashsalt':'spawned-fede-research'})
    figures=[]
    def save(fig,name):
        fig.tight_layout(rect=(0,0,1,.95))
        path=out/'graphs'/f'{name}.png';fig.savefig(path,bbox_inches='tight')
        svg=path.with_suffix('.svg');fig.savefig(svg,bbox_inches='tight',metadata={'Date':None})
        svg.write_text('\n'.join(line.rstrip() for line in svg.read_text().splitlines())+'\n')
        plt.close(fig);figures.append(path)
        return f'![{name}](graphs/{name}.png)'
    summary=[];groups=defaultdict(list)
    for row in good:
        c=row['config']
        key=(row['phase'],int(row['workers']),row['variant'],c.get('topology',''),c.get('layout',''),int(c.get('rate',0)),int(c.get('burst',1)),c.get('work',''),c.get('routing',''),int(c.get('actors',0)),int(c.get('iterations',0)),int(c.get('messages',0)))
        groups[key].append(row)
    for (phase,workers,variant,topology,layout,rate,burst,work,routing,actors,iterations,messages),group in sorted(groups.items()):
        rec=dict(phase=phase,workers=workers,variant=variant,topology=topology,layout=layout,rate=rate,burst=burst,work=work,routing=routing,actors=actors,iterations=iterations,messages=messages,
                 samples=len(group),valid_driver_samples=sum(valid(r) for r in group))
        for key in METRICS:
            value=stats(group,key)
            if value:
                for suffix,v in zip(['median','min','max'],value):rec[f'{key}_{suffix}']=v
        for label,numer,denom in [('cold_deadline_miss_pct','cold_late_observed','cold_samples'),('hot_timeout_pct','hot_timeouts','hot_samples'),('timeout_pct','timeouts','messages')]:
            values=[100*number(r,numer)/number(r,denom) for r in group if number(r,denom)>0]
            if values:rec[label]=statistics.median(values)
        if rate:
            ref=meta['calibration'][str(workers)]['service_reference_per_s'];rec['reference_ratio']=rate/ref
        summary.append(rec)
    fields=list(dict.fromkeys(k for r in summary for k in r))
    with (out/'SUMMARY.csv').open('w') as file:
        writer=csv.DictWriter(file,fields,lineterminator='\n');writer.writeheader();writer.writerows(summary)
    counts={phase:sum(r['phase']==phase for r in rows) for phase in sorted({r['phase'] for r in rows})}
    doc=['# Spawned: profiling, actor graphs and overload fairness on native Linux','',
         f"Source `{meta['revision']}`; dirty `{meta['dirty']}`. {len(rows):,} fresh processes; {len(good):,} successful. Phase counts: `{counts}`.",'',
         f"{meta['platform']}; {meta['rustc']}; nice={meta['nice']}. Actors inherit CPUs `{meta['actor_cpus']}`; new graph/load producers use `{meta['driver_cpus']}` and collectors/samplers use `{meta.get('collector_cpus','shared with producers')}`. Both budgets use the same four-core actor affinity mask; workers are not individually pinned. Other users and frequency scaling remain uncontrolled.",'',
         '[Protocol and limitations](../../RESEARCH-FEDE-PROTOCOL.md) · [Raw inputs and exact schedule](../../results/fede-research-2026-11-06/metadata.json)','',
         'The earlier 9b22346 study is separate: these workloads use corrected source, different graphs/load and explicit driver CPU separation. Compare policies inside each case. Bars/lines are medians; whiskers/ranges show fresh-process min/max, not confidence intervals.','']
    doc.extend(['## Actor-to-actor graphs','','Each logical request traverses eight actual handlers (chain or seven-way fan-out). Short graph cells use 65,536 requests; graph-long confirmations use 1,048,576 and are kept separate. The logical graph, count and work are identical across placement controls. Same/cross placement is enforced only for sharded actors; under Tokio it is a spawn-order control. Producers await each root reply.',''])
    for graph_phase,w in sorted({(r['phase'],int(r['workers'])) for r in rows if r['phase'] in ['graph','graph-long']}):
        for metric,factor,label,name in [('replies_per_s',1e-3,'k logical root replies / second','throughput'),('observed_p99_ns',1e-6,'Caller-observed root p99 (ms)','latency')]:
            fig,axes=plt.subplots(2,2,figsize=(14,9))
            for ax,(topology,layout) in zip(axes.flat,[(t,l) for t in ['chain','fanout'] for l in ['same','cross']]):
                for i,(variant,color) in enumerate(zip(VARIANTS,COLORS)):
                    g=[r for r in good if r['phase']==graph_phase and int(r['workers'])==w and r['variant']==variant and r['config']['topology']==topology and r['config']['layout']==layout]
                    value=stats(g,metric,factor)
                    if value:
                        mid,lo,hi=value;ax.barh(i,mid,color=color,xerr=[[mid-lo],[hi-mid]],capsize=2)
                ax.set_yticks(range(len(VARIANTS)),VARIANTS);ax.invert_yaxis();ax.set_xlim(left=0);ax.set_xlabel(label);ax.set_title(f'{topology}, {layout} placement')
            fig.suptitle(f'{graph_phase} · {w} actor workers · 1024 actors · 256 producers · eight handlers per request')
            doc.extend([save(fig,f'{graph_phase}-w{w}-{name}'),''])
    doc.extend(['## Graph worker CPU counters','','These counters include the timed actor work and drain, not producer CPU. CPU ns/root sums worker CPU across threads, so it can exceed elapsed ns/root. Run-queue counters depend on kernel availability; no migration/cache-event attribution is made.','',
        '| Long graph | Policy | Actor CPU ns/root | CPU ns/handler | Voluntary switches/root |',
        '|---|---|---:|---:|---:|'])
    for r in summary:
        if r['phase']=='graph-long':
            doc.append(f"| {r['topology']} / {r['layout']} | {r['variant']} | {r.get('actor_cpu_ns_per_root_median',0):,.0f} | {r.get('actor_cpu_ns_per_handler_median',0):,.0f} | {r.get('actor_voluntary_switches_per_root_median',0):.3f} |")
    doc.append('')
    doc.extend(['## Offered-load fairness','','Fixed hot/cold traffic, steady and burst schedules. Quantiles of successful completions omit timed-out requests; observation tails include timeout events with censored completion. Timeout/deadline fractions must accompany latency. A point is marked × if any of its three repetitions has an invalid generator schedule (>1% requests >1 ms late). Do not rank such points as equivalent offered traffic. These two-second finite windows are not a soak or SLO test.',''])
    for w in sorted({int(r['workers']) for r in rows}):
        for layout in ['spread','concentrated']:
            fig,axes=plt.subplots(2,4,figsize=(20,9.5))
            charts=[('cold_other_observed_p99_ns_median',1e-6,'Other-cohort cold observed p99 (ms)'),
                    ('cold_deadline_miss_pct',1,'Cold observed deadline misses (%)'),
                    ('timeout_pct',1,'All caller timeouts (%)'),
                    ('cold_p99_ns_median',1e-6,'Cold completion p99, survivors (ms)'),
                    ('lag_p99_ns_median',1e-6,'Producer admission lag p99 (ms)'),
                    ('observer_lag_p99_ns_median',1e-6,'Reply observation lag p99 (ms)'),
                    ('peak_queued_median',1,'Sampled peak mailbox queue'),
                    ('window_service_per_s_median',1e-3,'In-window service (k handlers/s)')]
            for ax,(metric,factor,title) in zip(axes.flat,charts):
                for variant,color in zip(VARIANTS,COLORS):
                    g=sorted([r for r in summary if r['phase']=='offered' and r['workers']==w and r['variant']==variant and r['layout']==layout and r['burst']==1],key=lambda r:r['rate'])
                    g=[r for r in g if metric in r]
                    if not g:continue
                    x=[r['rate']/1000 for r in g];y=[r[metric]*factor for r in g]
                    ax.plot(x,y,'o-',color=color,label=variant,markersize=4)
                    for a,b,r in zip(x,y,g):
                        if r['valid_driver_samples']!=r['samples']:ax.scatter([a],[b],marker='x',color='black',s=55,zorder=5)
                ax.set_title(title);ax.set_xlabel('Absolute offered rate (k requests/s)');ax.set_ylim(bottom=0)
            axes[0,0].legend(fontsize=8);fig.suptitle(f'{w} workers · hot actors {layout} · 128 actors, four hot, 80/20 traffic')
            doc.extend([save(fig,f'offered-w{w}-{layout}'),''])
            fig,axes=plt.subplots(1,3,figsize=(16,4.8))
            for variant,color in [('async',COLORS[0]),('sharded-1',COLORS[4]),('sharded-16',COLORS[5])]:
                choices=[r for r in good if r['phase']=='offered' and int(r['workers'])==w and r['variant']==variant and r['config']['layout']==layout and r['config'].get('burst',1)==1 and int(r['repeat'])==1]
                if not choices:continue
                row=max(choices,key=lambda r:int(r['config']['rate']))
                series=read_csv(args.data/'series'/f'{row["stem"]}.csv');x=[number(r,'elapsed_ms')/1000 for r in series]
                for ax,key,factor,title in zip(axes,['queued','cold_queued','rss_bytes'],[1,1,1e-6],['Mailbox queue','Cold mailbox queue','Process RSS (MB)']):
                    ax.plot(x,[number(r,key)*factor for r in series],color=color,label=f"{variant}: {'valid' if valid(row) else 'INVALID schedule'}, lag p99 {number(row,'lag_p99_ns')/1e6:.3f} ms");ax.set_title(title);ax.set_xlabel('Seconds since intended start')
                end=int(row['config']['messages'])/int(row['config']['rate'])
                for ax in axes:ax.axvline(end,color=color,alpha=.25,linestyle=':')
            for ax in axes:ax.set_ylim(bottom=0)
            axes[0].legend(fontsize=7);fig.suptitle(f'{w} workers · {layout} · 125% reference, predetermined repetition 1; dotted injection end\nInvalid schedules are diagnostics, not equivalent offered traffic')
            doc.extend([save(fig,f'timeseries-w{w}-{layout}'),''])
    doc.extend(['## Per-cell offered outcomes','','Survivor-only completion p99 is shown beside timeout counts. Caller-observed deadline misses also include late ready replies; an elapsed timeout does not cancel actor work. All admitted work drains before process exit. `valid/n` describes generator schedules, not successful replies.','',
        '| Workers | Layout | Burst | Policy | Rate/s | valid/n | Other-cohort cold observed p99 ms | Worst cold actor p99 ms | Cold deadline misses % | Hot timeouts % | Peak queued |',
        '|---:|---|---:|---|---:|---:|---:|---:|---:|---:|---:|'] )
    for r in summary:
        if r['phase']=='offered':
            doc.append(f"| {r['workers']} | {r['layout']} | {r['burst']} | {r['variant']} | {r['rate']:,} | {r['valid_driver_samples']}/{r['samples']} | {r.get('cold_other_observed_p99_ns_median',0)/1e6:.3f} | {r.get('worst_cold_actor_observed_p99_ns_median',0)/1e6:.3f} | {r.get('cold_deadline_miss_pct',0):.2f} | {r.get('hot_timeout_pct',0):.2f} | {r.get('peak_queued_median',0):,.0f} |")
    doc.extend(['','## Qualitative CPU profiles','','SIGPROF sampling was used because perf is restricted. Counts represent user-space samples; they do not measure hardware events or off-CPU causality. The profiler blocklists libc/libgcc/pthread/vdso, so missing samples cannot establish absence of those costs. Legacy CPU/hotspot/TCP profiles retain all threads in CPUs 2–5; graph profiles separate driver CPUs. Profiled rows are excluded from performance medians.',''])
    profile_rows=[];categories={'synthetic_handler':['workload_research::cpu_work','designs::process_at'],
        'scheduler':['tokio::runtime::scheduler'], 'mailbox':['tokio::sync::mpsc'],
        'reply':['tokio::sync::oneshot'], 'network':['tokio::net','mio::','tokio::io'],
        'allocation_frames':['alloc::alloc','alloc::raw_vec','malloc','realloc']}
    doc.extend(['Shares below are inclusive: a stack may contain several categories. The denominator is captured actor-worker samples, not elapsed wall time or all process CPU. Separate flamegraphs retain driver/main stacks. Scheduler ancestry appears in essentially every actor stack, including time executing user code: a 100% inclusive scheduler share does not mean 100% scheduling overhead.','',
        '| Case / workers / policy / repetition | Actor samples | Other samples | Scheduler % | Mailbox % | Reply % | Network % | Flamegraph |',
        '|---|---:|---:|---:|---:|---:|---:|---|'])
    for row in [r for r in good if r['phase']=='profile']:
        path=args.data/'profiles'/f'{row["stem"]}.stacks';counts=defaultdict(int);actor=0;other=0
        for line in path.read_text().splitlines():
            stack,count=line.rsplit(' ',1);count=int(count);thread=stack.split(';')[0]
            is_actor=thread.startswith(('actor-worker','spawned-shard-','tokio-runtime-w'))
            if not is_actor:other+=count;continue
            actor+=count
            for category,patterns in categories.items():
                if any(pattern in stack for pattern in patterns):counts[category]+=count
        c=row['config']
        case=('graph-'+c['layout']) if c.get('mode')=='graph' else ('hotspot' if c.get('routing')=='hotspot' else c.get('work','cpu'))
        record=dict(case=case,stem=row['stem'],workers=row['workers'],variant=row['variant'],actor_samples=actor,other_samples=other)
        record.update({k+'_pct':100*counts[k]/actor if actor else '' for k in categories})
        profile_rows.append(record)
        pct=lambda key:f'{100*counts[key]/actor:.1f}' if actor else 'unavailable'
        link=f'../../results/fede-research-2026-11-06/profiles/{row["stem"]}.svg'
        doc.append(f"| {case} / {row['workers']} / {row['variant']} / {row['repeat']} | {actor:,} | {other:,} | {pct('scheduler')} | {pct('mailbox')} | {pct('reply')} | {pct('network')} | [SVG]({link}) |")
    if profile_rows:
        with (out/'PROFILE-SUMMARY.csv').open('w') as file:
            writer=csv.DictWriter(file,list(profile_rows[0]),lineterminator='\n');writer.writeheader();writer.writerows(profile_rows)
    doc.extend(['','## Interpretation limits','','The workload includes instrumentation and synthetic CPU work, and records finite unbounded mailboxes. Process RSS contains driver sample storage and pending replies; it is not isolated actor memory. Measured queue snapshots can miss short peaks. Shared-host results and short runs support specific follow-up experiments, not a default-backend promotion. Keep Tokio default until compatibility, long-run fairness and workload-specific evidence justify a supported alternative.',''])
    report='\n'.join(doc)+'\n';(out/'REPORT.md').write_text(report)
    body=markdown.markdown(report,extensions=['tables','fenced_code'])
    for path in figures:body=body.replace(f'src="graphs/{path.name}"',f'src="data:image/png;base64,{base64.b64encode(path.read_bytes()).decode()}"')
    html='''<!doctype html><html lang="en"><meta charset="utf-8"><meta name="viewport" content="width=device-width"><title>Spawned native Linux research</title><style>body{max-width:1400px;margin:40px auto;padding:0 24px;background:#fcfcfa;color:#203443;font:16px/1.6 system-ui}h1,h2,h3{line-height:1.2}h2{margin-top:2.5em;border-bottom:2px solid #d4e0e8;padding-bottom:.4em}img{width:100%;height:auto}table{display:block;overflow:auto;border-collapse:collapse;font-size:12px;width:100%}th,td{padding:7px;border-bottom:1px solid #d4e0e8;text-align:left}th{background:#e9f0f3}pre{background:#eef2f3;overflow:auto;padding:12px}a{color:#176a86}</style><body>'''+body+'</body></html>'
    (out/'REPORT.html').write_text(html)
    print(f'Rendered {len(figures)} figures, {len(summary)} aggregate cells, {len(profile_rows)} profiles')


if __name__=='__main__':
    main()
