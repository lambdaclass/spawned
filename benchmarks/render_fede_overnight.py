#!/usr/bin/env python3
"""Render version-separated native executor, driver and lifecycle experiments."""
import argparse
import base64
from collections import Counter, defaultdict
import csv
import json
from pathlib import Path
import statistics as st
import matplotlib
matplotlib.use('Agg')
import matplotlib.pyplot as plt
import markdown
import numpy as np


def rows(path):
    with path.open() as f: return list(csv.DictReader(f))


def num(r, k, default=0):
    return float(r[k]) if r.get(k) not in [None, ''] else default


def valid(r):
    return str(r.get('driver_valid')).lower() in ['true', '1']


def write(path, records):
    if not records: return
    with path.open('w') as f:
        w=csv.DictWriter(f, list(dict.fromkeys(k for r in records for k in r)), lineterminator='\n')
        w.writeheader(); w.writerows(records)


def stats(g, k, scale=1):
    vals=[num(r,k)*scale for r in g if r.get(k) not in [None,'']]
    return (st.median(vals), min(vals), max(vals)) if vals else None


def label(r):
    value=f"{r['variant']} N{r['workers']}" + ('+R1' if r['variant'].startswith('native') else '') + (f" Q{r.get('quantum',8)}" if r['variant'] in ['native-root','native-runnable','native-deque'] else '')
    if r.get('zero_schedule') not in [None,''] and r['variant'] in ['native-runnable','native-deque']:value+=f" Z{r['zero_schedule']}"
    if r.get('wake_precheck') not in [None,''] and r['variant'].startswith('native'):value+=f" W{r['wake_precheck']}"
    if r.get('inline_future') not in [None,''] and r['variant'] in ['native-runnable','native-deque']:value+=f" I{r['inline_future']}"
    return value


METRICS=['replies_per_s','observed_p99_ns','completion_p99_ns','cold_other_observed_p99_ns',
 'worst_cold_actor_observed_p99_ns','cold_p99_ns','hot_p99_ns','lag_p99_ns','observer_lag_p99_ns',
 'peak_queued','peak_cold_queued','peak_hot_queued','peak_outstanding','peak_rss_bytes','rss_bytes',
 'window_service_per_s','window_queue_growth_per_s','cpu_s','elapsed_s','wall_s','timeouts',
 'hot_timeouts','cold_timeouts','cold_miss_pct','hot_timeout_pct','timeout_pct',
 'executor_cpu_ns_per_root','executor_voluntary_switches_per_root','executor_runqueue_wait_ns_per_root',
 'cycles','started','stopped','dropped','requests','down']


def main():
    p=argparse.ArgumentParser(description=__doc__)
    p.add_argument('--data',type=Path,required=True);p.add_argument('--output',type=Path,required=True)
    a=p.parse_args(); out=a.output;out.mkdir(parents=True,exist_ok=True);(out/'graphs').mkdir(exist_ok=True)
    raw=rows(a.data/'raw.csv'); meta=json.loads((a.data/'metadata.json').read_text())
    assert raw and all(r['status']=='ok' for r in raw)
    for r in raw:
        c=json.loads(r['config_json']);e=json.loads(r.get('experiment_json') or '{}')
        r['config']=c;r['experiment']=e;r['phase']=r.get('phase') or 'soak'
        r.update(topology=c.get('topology',''),layout=c.get('layout',''),caller=c.get('caller','blocking'),
                 counter=c.get('observation-counter','on'),quantum=int(e.get('SPAWNED_EXPERIMENT_ROOT_QUANTUM',8)),zero_schedule=int(e['SPAWNED_EXPERIMENT_ZERO_SCHEDULE']) if 'SPAWNED_EXPERIMENT_ZERO_SCHEDULE' in e else '',wake_precheck=int(e['SPAWNED_EXPERIMENT_WAKE_PRECHECK']) if 'SPAWNED_EXPERIMENT_WAKE_PRECHECK' in e else '',inline_future=int(e['SPAWNED_EXPERIMENT_INLINE_FUTURE']) if 'SPAWNED_EXPERIMENT_INLINE_FUTURE' in e else '',slice_us=int(e.get('SPAWNED_EXPERIMENT_SLICE_US',50)),
                 actors=int(c.get('actors',0)),producers=int(c.get('producers',0)),rate=int(c.get('rate',0)),burst=int(c.get('burst',1)))
        for metric, numerator, denominator in [('cold_miss_pct','cold_late_observed','cold_samples'),('hot_timeout_pct','hot_timeouts','hot_samples'),('timeout_pct','timeouts','messages')]:
            if num(r,denominator):r[metric]=100*num(r,numerator)/num(r,denominator)
        path=a.data/'threads'/f"{r['stem']}.csv"
        if path.exists() and num(r,'messages'):
            t=rows(path)
            r['executor_cpu_ns_per_root']=sum(num(x,'runtime_ns') for x in t)/num(r,'messages')
            r['executor_voluntary_switches_per_root']=sum(num(x,'voluntary_switches') for x in t)/num(r,'messages')
            r['executor_runqueue_wait_ns_per_root']=sum(num(x,'runqueue_wait_ns') for x in t)/num(r,'messages')
    groups=defaultdict(list)
    for r in raw:
        # Preserve ALL workload and experiment settings, ignoring artifact destination paths only.
        config={k:v for k,v in r['config'].items() if k not in ['series','threads','profile','stacks']}
        key=(r['dataset'],r.get('phase','soak'),r['variant'],r['workers'],json.dumps(config,sort_keys=True),json.dumps(r['experiment'],sort_keys=True))
        groups[key].append(r)
    summary=[]
    for key,g in sorted(groups.items()):
        r=g[0];s={k:r[k] for k in ['dataset','revision','variant','workers','topology','layout','caller','counter','slice_us','quantum','zero_schedule','wake_precheck','inline_future','actors','producers','rate','burst']}
        s.update(phase=r.get('phase','soak'),samples=len(g),valid_driver_samples=sum(valid(x) for x in g),config_json=key[4],experiment_json=key[5])
        for metric in METRICS:
            value=stats(g,metric)
            if value:
                s.update({metric+'_'+suffix:v for suffix,v in zip(['median','min','max'],value)})
        summary.append(s)
    write(out/'SUMMARY.csv',summary)
    plt.rcParams.update({'font.size':9,'axes.spines.top':False,'axes.spines.right':False,'axes.grid':True,'grid.alpha':.2,'figure.dpi':135,'svg.hashsalt':'spawned-overnight'})
    figures=[];palette=plt.get_cmap('tab20')
    def save(fig,name):
        fig.tight_layout(rect=(0,0,1,(.75 if len(fig.axes)==3 else .86) if fig.legends else .93));path=out/'graphs'/f'{name}.png'
        fig.savefig(path,bbox_inches='tight');svg=path.with_suffix('.svg');fig.savefig(svg,bbox_inches='tight',metadata={'Date':None})
        svg.write_text('\n'.join(x.rstrip() for x in svg.read_text().splitlines())+'\n');plt.close(fig);figures.append(path)
        return f'![{name}](graphs/{name}.png)'
    def bars(block,phase,caller,actors,producers,metric,scale,title):
        chosen=[s for s in summary if s['dataset']==block and s['phase']==phase and s['caller']==caller and s['counter']=='on' and s['actors']==actors and s['producers']==producers]
        labels=sorted({label(s) for s in chosen})
        fig,axes=plt.subplots(2,2,figsize=(16,12))
        for ax,(topology,layout) in zip(axes.flat,[(x,y) for x in ['chain','fanout'] for y in ['same','cross']]):
            for i,l in enumerate(labels):
                g=[s for s in chosen if label(s)==l and s['topology']==topology and s['layout']==layout]
                assert len(g)<=1,(block,caller,l,'would pool unlike groups')
                if not g:continue
                s=g[0];mid=s[metric+'_median']*scale;lo=s[metric+'_min']*scale;hi=s[metric+'_max']*scale
                ax.barh(i,mid,xerr=[[mid-lo],[hi-mid]],color=palette(i%20),capsize=2)
            ax.set_yticks(range(len(labels)),labels);ax.invert_yaxis();ax.set_xlim(left=0);ax.set_xlabel(title);ax.set_title(f'{topology} / {layout} ID layout')
        cores=8 if phase in ['runnable-scale','deque-scale','inline-scale','fair-scale'] else 4
        fig.suptitle(f'{block} · {caller} caller · {actors:,} actors / {producers:,} producers / 8 handlers per root\n{cores} physical actor cores; native N + one I/O worker, plus idle owner\nSame/cross enforces placement only for static shards; dynamic schedulers may migrate')
        return save(fig,f'{block}-{phase}-{caller}-a{actors}-p{producers}-{metric}')
    doc=['# Spawned overnight backend experiments on fede256','',
         f"{len(raw):,} successful fresh processes archived in {len(meta['inputs'])} completed blocks. Original source revisions, binary hashes, exact schedules and inputs remain separate. **Tokio remains the default; these are private research backends.**",'',
         '[Machine-readable summary](SUMMARY.csv) · [Raw inputs and hashes](../../results/fede-overnight-2026-11-06/metadata.json) · [Experiment protocol](../../OVERNIGHT-PROTOCOL.md) · [Correctness findings](../../CORRECTNESS-FEDE-OVERNIGHT.md)','',
         '## What each backend does','',
         '| Backend | Scheduling / execution | Main tradeoff |','|---|---|---|',
         '| async | Existing Tokio multi-thread runtime; ordinary actor task and mailbox | Mature scheduler and compatibility; reference |',
         '| tokio-16 | Same Tokio scheduling, explicit 16-message / 50 µs cooperative turns | Yield checks trade throughput for fairness; no running-handler preemption |',
         '| tokio-pool | Process-owned Tokio runtime with fixed worker count | Separates runtime ownership from actor scheduling |',
         '| sharded-16 | Actor assigned to one single-thread runtime; 16-message / 50 µs turns | Locality helps colocated traffic; remote graph edges and hotspots can hurt |',
         '| native-global | Custom actor jobs on a shared FIFO guarded by one mutex; Tokio I/O bridge per job poll | Simple work sharing; queue contention and runtime-entry cost |',
         '| native-local | Local LIFO with periodic oldest stealing, all queues under one mutex | Local continuation preference; shared lock remains |',
         '| native-steal | Separate mutex/deque per worker, local LIFO and oldest stealing, park/unpark | Less queue contention; still custom job/refcount and per-poll bridge overhead |',
         '| native-root | Same split queues, direct job polls inside one root future per native worker; yield root after a configurable poll quantum (default 8) | Amortizes runtime entry; still depends on Tokio for root driving and shared I/O |','',
         'Native-runnable replaces the custom job Gate/future mutex with async-task4.7.1 Runnable tickets, retaining a boxed actor future and separate poll/disposal unwind boundaries. Native-deque adds Crossbeam0.8.8 owner deques, shared outside injection and batch stealing. The latter changes queue primitive and scheduling policy together, so improvements cannot isolate mutex cost. Both reuse the root driver/shared Tokio I/O; these are not Tokio-free runtimes.', '',
         'Figure key: N = actor scheduler workers, R1 = one additional shared I/O worker (idle owner/helpers are additional threads), Q = job polls per outer-driver turn, Z = zero-sized metadata schedule callback, W = sleeping-peer precheck, I = generic inline future storage. One root reply covers eight handler invocations; roots/s is not send-only messages/s. For example, 16,128 actors share eight actor worker threads in the scale block; there is no OS thread per actor in these scheduler experiments.', '',
         'All keep addressed, unbounded private mailboxes, boxed ordinary Rust messages and request/reply. No lossy broadcast ring, Copy-only restriction, public Actor/Handler/Backend API change or default promotion. Native actor jobs are not Tokio tasks. Native-root uses a current-thread root driver per native worker but no additional OS thread for that driver; block_in_place is unsupported and actor task IDs are absent.','',
         '## Correctness discoveries and source boundaries','',
         '| Version | Scope | Known limitation / disposition |','|---|---|---|',
         '| V1 07f0e11 | Initial global/local native queues; 72 smokes + 360 graph processes | Stronger Loom publication model later failed: a coalesced wake did not publish prior writes. Remaining stages cancelled. Historical observations only. |',
         '| V2 eefc302 | Publication corrected; split queues, caller/counter factorial; 728 processes | Review later found a possible Global idle-shutdown lost notify. Remaining stages cancelled after callers. Historical observations only. |',
         '| V3 097ae02 | Corrected shutdown predicate + serialized teardown; adds amortized root driver | Separate common-source controls and gates; not proof of a supported production executor. |','',
         '| V4 09dd194 | Root/Runnable/quanta on stop-corrected source | Offered/resource evidence separate; unstarted480-case scale sweep deferred for the fairness correction. |',
         '| V5 329006e | Crossbeam owner-deque transport/policy | Review found possible local-oldest starvation; unstarted follow-ups cancelled. |',
         '| V6 0f81b57 | Metadata callback and wake-precheck factorial | Deque retains V5 fairness limitation; Root/Runnable unaffected. |',
         '| V7 acd37a3 | Generic pinned inline/boxed future pairs | Deque retains fairness limitation; inline throughput did not justify complexity. |',
         '| V8 1dcfea1 | Alternating oldest/injection priority, safe hint fallback, concurrent conservation tests | Corrected frozen source; bounded tests remain different from formal/production guarantees. |',
         'The publication fix requires a release RMW even when wake state remains QUEUED or RUNNING|NOTIFIED. Queue-count-only Loom tests missed this visibility defect. The Global shutdown fix changes its closed predicate under the same mutex as Condvar wait. Concurrent shutdown callers serialize teardown. Older timings are not pooled into V3 medians.','',
         'Later PR187 review found a Deque fairness defect: every periodic oldest turn prioritized injection, allowing sustained external arrivals plus hot local continuations to starve the local bottom. V5/V6/V7 Deque observations remain historical; Root/Runnable are unaffected. Unstarted V5 and V7 optimized follow-ups were cancelled before scoring. V8 (1dcfea1; initial corrected preflight e633e59) alternates local-oldest/injection priority, adds owner-pop fallback after bounded retries and validates TLS worker identity. The strengthened regression fails on old95b4ca6 and passes on the correction; a preliminary probe missing hot local continuations had passed and is retained as a test-design limitation. No formal hard-latency fairness guarantee follows.', '',
         'V7 pairs boxed/dynamic actor future storage (I0) with a generic pinned inline future (I1) inside async-task. It uses safe pin projection and guarded in-place disposal, retaining ordinary message/envelope/reply allocations. The comparison changes layout/allocation and monomorphization together. A task allocation can retain inline storage while stale Wakers live; graph RSS is not an isolated actor-memory measurement. Public Actor/Handler/Backend signatures and Tokio default stay unchanged.', '',
         'V6 (source recorded per block) pairs zero-sized metadata schedule callbacks (Z0/1) and a no-sleeping-peer precheck (W0/1) on one common source. It targets captured-schedule reference traffic and a shared rotating-target RMW. Graph/profile evidence does not establish mixed-load wake tails or production readiness.', '',
         'V4 (09dd194) includes the production stop fix, shared selector correction and Runnable/quanta controls. V5 (329006e) adds owner deques/batch injection on another common source. Earlier V4 preflights were stopped before scoring: the scale harness initially rejected >256 producers, then inspection found missing owned-runtime/test selectors. Both were corrected, gated and smoked on a new source; old preflight outcomes are not resource-budget rankings.', '',
         'A separate deterministic stop race was reproduced in both production actor modes and fixed in [PR185](https://github.com/lambdaclass/spawned/pull/185). V3 timings remain on their recorded pre-PR185 source; they are not claimed as measurements of current main.','',
         '## Machine and measurement','',
         'Native Linux fede256 (SSH alias fede), Ryzen 9 9950X3D, 16 physical / 32 logical CPUs, about 251 GiB RAM. Rust 1.93, nice 10. Actor threads inherit CPUs 2–5; producers 6,7,14,15; collectors 8,9,16,17. These are distinct physical cores without own SMT pairing. Workers are not individually pinned. All actor budgets, including five runtime workers, share the same four actor cores. Shared-host activity and frequency scaling remain uncontrolled.','',
         'Four-core blocks use the masks above. Eight-core scale blocks use actor CPUs2–9, producers10–13 and collectors14–17, with1,024 logical producers on four driver runtime threads; V4 also includes512 producers. Those16CPU IDs are distinct physical cores. The eight-core mask spans two L3 domains: CPUs2–7 share96MiB L3, CPUs8–9 share32MiB L3 withothercores. Four-core actors stay in the96MiB domain. This is not a homogeneous-core strong-scaling experiment, and no cache-event cause is inferred. The archived cpu-cache-topology.json records per-CPU domains/SMT siblings. Their two-million-root requests and actor populations remain separate from the four-core blocks. Native N7+R1 versus Tokio8 and N8+R1 versus Tokio9 expose corresponding worker controls on eight actor cores. Native N3+R1 versus Tokio N4 controls total runtime worker count; native N4+R1 versus Tokio N4 controls actor worker count, and versus Tokio N5 controls total runtime workers. The native idle owner thread and lazily created blocking helpers are additional and disclosed. Graphs use 1,920 actors, 256 logical producers, 1,048,576 roots and eight handlers/root with 100 synthetic CPU iterations/handler. Same/cross placement is enforced only for static shards; elsewhere it is a spawn-order control.','',
         'Blocking callers use 256 native producer threads; async callers use four current-thread runtimes driving 256 logical producers. Both use the same four producer cores and one outstanding root per producer. Their total thread populations differ. Observation-counter off removes one shared atomic, not clocks, latency storage, allocation or all instrumentation.','',
         'Bars/points show medians. Matched graph bars include fresh-process min/max whiskers; all ranges are in SUMMARY.csv, not confidence intervals. Profiled runs, smokes, soak smokes, different versions, caller modes, counters and slices remain separate.','',
         '## Completed blocks','', '| Block | Source | Processes |','|---|---|---:|']
    for block,info in meta['inputs'].items():doc.append(f"| {block} | `{info['revision'][:12]}` | {info['samples']} |")
    findings=Path(__file__).with_name('FINDINGS-FEDE-OVERNIGHT.md')
    if findings.exists():doc[6:6]=['',findings.read_text(),'']
    doc.extend(['','## Matched graph results',''])
    graph_phases=['native-graphs','control-queues','root-graphs','runnable-graphs','runnable-scale','deque-graphs','deque-scale','callback-graphs','inline-graphs','inline-scale','fair-graphs','fair-scale']
    for block,phase,caller,actors,producers in sorted({(r['dataset'],r['phase'],r['caller'],r['actors'],r['producers']) for r in raw if r['phase'] in graph_phases and r['counter']=='on'}):
        for metric,scale,title in [('replies_per_s',1e-3,'k logical root replies / s'),('observed_p99_ns',1e-6,'Caller-observed root p99 (ms)')]:doc.extend([bars(block,phase,caller,actors,producers,metric,scale,title),''])
    doc.extend(['## Graph numbers','', '| Block | Actors/producers | Shape | Caller/counter | Policy | n | Root replies/s | Root p99 ms | Executor CPU ns/root |','|---|---|---|---|---|---:|---:|---:|---:|'])
    for s in summary:
        if s['phase'] in graph_phases:
            doc.append(f"| {s['dataset']} | {s['actors']}/{s['producers']} | {s['topology']}/{s['layout']} | {s['caller']}/{s['counter']} | {label(s)} | {s['samples']} | {s['replies_per_s_median']:,.0f} | {s['observed_p99_ns_median']/1e6:.3f} | {s.get('executor_cpu_ns_per_root_median',0):,.0f} |")
    doc.extend(['','Executor CPU sums all recorded runtime threads, including I/O/owner/blocking helpers; producer threads are excluded. It includes the start gate and drain; timed root elapsed excludes start padding. CPU/root can exceed wall ns/root. Whole-process CPU/RSS additionally include producers, samples, startup and sorting. Kernel schedstat is not a migration/cache measurement.','', '## Caller and counter controls',''])
    for block in sorted({s['dataset'] for s in summary if s['phase']=='control-callers'}):
        labels=sorted({label(s) for s in summary if s['dataset']==block and s['phase']=='control-callers'});fig,axes=plt.subplots(2,2,figsize=(16,10))
        for ax,(topology,layout) in zip(axes.flat,[(x,y) for x in ['chain','fanout'] for y in ['same','cross']]):
            for j,(caller,counter) in enumerate([(x,y) for x in ['blocking','async'] for y in ['on','off']]):
                g=[s for s in summary if s['dataset']==block and s['phase']=='control-callers' and s['caller']==caller and s['counter']==counter and s['topology']==topology and s['layout']==layout]
                by={label(s):s for s in g}
                assert len(by)==len(g) and set(by)==set(labels),(block,caller,counter,topology,layout,'factorial must be complete and unique')
                x=np.arange(len(labels))+(j-1.5)*.18
                ax.bar(x,[by[l]['replies_per_s_median']/1000 for l in labels],width=.18,label=f'{caller}, counter {counter}')
            ax.set_xticks(range(len(labels)),labels,rotation=15);ax.set_ylabel('k root replies / s');ax.set_title(f'{topology}/{layout}');ax.set_ylim(bottom=0)
        fig.legend(*axes[0,0].get_legend_handles_labels(),loc='upper center',bbox_to_anchor=(.5,.94),ncol=4,fontsize=8);fig.suptitle(f'{block}: caller / instrumentation factorial; medians of five fresh processes')
        doc.extend([save(fig,block+'-factorial'),''])
    doc.extend(['## Count-only versus time-limited turns','','Slice 0 uses only the message-count budget; 25/50/200 µs also read the clock between completed handlers. Neither preempts a running handler. Tokio cooperative budgeting is separate. Empty mailboxes may never reach the explicit turn limit.',''])
    for block in sorted({s['dataset'] for s in summary if s['phase']=='control-clock'}):
        chosen=[s for s in summary if s['dataset']==block and s['phase']=='control-clock'];labels=sorted({label(s) for s in chosen});fig,axes=plt.subplots(2,2,figsize=(15,10))
        for ax,(topology,layout) in zip(axes.flat,[(x,y) for x in ['chain','fanout'] for y in ['same','cross']]):
            for i,l in enumerate(labels):
                g=sorted([s for s in chosen if label(s)==l and s['topology']==topology and s['layout']==layout],key=lambda s:s['slice_us'])
                if not g:continue
                if g[0]['variant']=='async':
                    assert len(g)==1,(block,l,'expected one default reference')
                    ax.axhline(g[0]['replies_per_s_median']/1000,color=palette(i%20),linestyle=':',label=l+' reference')
                else:ax.plot([s['slice_us'] for s in g],[s['replies_per_s_median']/1000 for s in g],'o-',color=palette(i%20),label=l)
            ax.set_xticks([0,25,50,200]);ax.set_xlabel('Slice µs (0 = count only)');ax.set_ylabel('k root replies / s');ax.set_ylim(bottom=0);ax.set_title(f'{topology}/{layout}')
        fig.legend(*axes[0,0].get_legend_handles_labels(),loc='upper center',bbox_to_anchor=(.5,.94),ncol=4,fontsize=8);fig.suptitle(f'{block}: async caller, counter on, 16-message explicit turn')
        doc.extend([save(fig,block+'-slice'),''])
    doc.extend(['## Offered-load fairness','','240 actors; four hot actors receive 80% of traffic, hot work 5,000 iterations and cold work 100. Spread/concentrated are hot actor ID layouts: static shards enforce worker placement, while pooled schedulers can move actors. Common absolute rates 500k, 900k and 1.2M requests/s; two-second injection windows; caller deadline two seconds; all admitted work drains. Steady and 1,000-request bursts remain separate. × means at least one repetition exceeded the unchanged validity threshold (>1% requests >1 ms late). Such points are diagnostics and cannot be ranked as equivalent offered traffic. Survivor completion p99 excludes timeouts; observed deadline miss fractions accompany censored observations.',''])
    for block,offered_phase in sorted({(s['dataset'],s['phase']) for s in summary if s['phase'] in ['control-offered','runnable-offered','deque-offered','frontier-offered']}):
        chosen=[s for s in summary if s['dataset']==block and s['phase']==offered_phase];labels=sorted({label(s) for s in chosen})
        for layout in ['spread','concentrated']:
            for burst in [1,1000]:
                fig,axes=plt.subplots(4,2,figsize=(15,17))
                metrics=[('cold_other_observed_p99_ns',1e-6,'Cold other-cohort observed p99 ms'),('cold_miss_pct',1,'Cold observed deadline misses %'),('timeout_pct',1,'All timeouts %'),('cold_p99_ns',1e-6,'Cold completion p99, survivors ms'),('lag_p99_ns',1e-6,'Admission lag p99 ms'),('observer_lag_p99_ns',1e-6,'Reply observation lag p99 ms'),('peak_queued',1e-6,'Peak queued, millions'),('window_service_per_s',1e-3,'In-window service, k/s')]
                for ax,(metric,scale,title) in zip(axes.flat,metrics):
                    for i,l in enumerate(labels):
                        g=sorted([s for s in chosen if label(s)==l and s['layout']==layout and s['burst']==burst and metric+'_median' in s],key=lambda s:s['rate'])
                        assert len({s['rate'] for s in g})==len(g),(block,l,'duplicate offered groups')
                        ax.plot([s['rate']/1000 for s in g],[s[metric+'_median']*scale for s in g],'o-',label=l,color=palette(i%20),markersize=3)
                        for s in g:
                            if s['valid_driver_samples']!=s['samples']:ax.scatter(s['rate']/1000,s[metric+'_median']*scale,marker='x',s=55,color='black',zorder=5)
                    ax.set_title(title);ax.set_xlabel('Absolute offered k requests/s');ax.set_ylim(bottom=0)
                fig.legend(*axes[0,0].get_legend_handles_labels(),loc='upper center',bbox_to_anchor=(.5,.94),ncol=4,fontsize=7);fig.suptitle(f'{block} · hot actors {layout} · burst {burst} · invalid schedule groups marked ×')
                doc.extend([save(fig,f'{block}-{offered_phase}-{layout}-burst{burst}'),''])
            # Fixed repetition zero, highest common rate, steady input; not chosen by latency.
            fig,axes=plt.subplots(1,3,figsize=(20,8))
            for i,l in enumerate(labels):
                options=[r for r in raw if r['dataset']==block and r['phase']==offered_phase and label(r)==l and r['layout']==layout and r['burst']==1 and int(r['repeat'])==0]
                if not options:continue
                assert len({r['rate'] for r in options})==len(options),(block,l,'duplicate time-series schedule')
                r=max(options,key=lambda r:r['rate']);ts=rows(a.data/'series'/f"{r['stem']}.csv")
                for ax,key,scale,title in zip(axes,['queued','cold_queued','rss_bytes'],[1e-6,1e-6,1e-6],['Queued (millions)','Cold queued (millions)','Whole-process RSS (MB)']):
                    ax.plot([num(t,'elapsed_ms')/1000 for t in ts],[num(t,key)*scale for t in ts],color=palette(i%20),label=f"{l}: {'valid' if valid(r) else 'INVALID'}, lag {num(r,'lag_p99_ns')/1e6:.3f} ms")
                    ax.set_title(title);ax.set_xlabel('Seconds since intended start')
            for ax in axes:ax.set_ylim(bottom=0);ax.axvline(2,color='gray',linestyle=':')
            fig.legend(*axes[0].get_legend_handles_labels(),loc='upper center',bbox_to_anchor=(.5,.88),ncol=3,fontsize=7);fig.suptitle(f'{block} / {layout}: 1.2M/s steady, predetermined repetition 0\nInvalid schedules are diagnostics, not equivalent offered traffic; dotted injection end')
            doc.extend([save(fig,f'{block}-{offered_phase}-{layout}-timeseries'),''])
        selected=[s for s in chosen];good=sum(s['valid_driver_samples'] for s in selected);total=sum(s['samples'] for s in selected)
        doc.extend([f'Valid schedules: **{good}/{total}**. All invalid runs remain in raw data and the per-cell summary.',''])
    doc.extend(['## Lifecycle and resources','','Closed-loop lifecycle churn maintains 1,024 idle residents and creates/stops 128 actors per cycle, with 16 stateful requests per actor, helper/timer awaits, exact monitor IDs/reasons and periodic intentional handler panic. Completed V3/V4 resource configurations and the corrected-source V8 follow-up request 300-second windows, chosen before their resource outcomes; exact requested durations remain in metadata. Unstarted V5 windows were cancelled after review found a fairness defect. Histories are bounded per cycle; 1 Hz resource samples stream to disk. All started/stopped/dropped counts must match at final join. These runs assess finite lifecycle/resource behavior, not production leak freedom, open-loop fairness, SLOs or comparable throughput. The first V3 resource attempt is excluded: pausing its Python pipe reader blocked diagnostic output. Its process/thread evidence is archived as a harness error; all ten cases are rerun in soaks-3 at nice10 with the reader continuously active. An intermediate nested-nice19 launch was cancelled before a completed case; both attempts remain harness diagnostics.',''])
    resources=[]
    for block in sorted({r['dataset'] for r in raw if r.get('phase','soak')=='soak' and 'smoke' not in r['dataset']}):
        chosen=[r for r in raw if r['dataset']==block and r['phase']=='soak']
        assert len({label(r) for r in chosen})==len(chosen),(block,'duplicate resource labels')
        fig,axes=plt.subplots(2,2,figsize=(18,10))
        for i,r in enumerate(chosen):
            ts=rows(a.data/'series'/f"{r['stem']}.csv");tail=[t for t in ts if num(t,'elapsed_s')>=60]
            rec=dict(dataset=block,policy=label(r),source=r['revision'],requested_seconds=r['config'].get('seconds',''),cycles=r['cycles'],started=r['started'],stopped=r['stopped'],dropped=r['dropped'],requests=r['requests'],down=r['down'],samples=len(ts))
            for key in ['rss_bytes','fds','threads']:
                rec[key+'_first']=num(ts[0],key);rec[key+'_last']=num(ts[-1],key);rec[key+'_max']=max(num(t,key) for t in ts)
                if tail:rec[key+'_after60_min']=min(num(t,key) for t in tail);rec[key+'_after60_max']=max(num(t,key) for t in tail)
            resources.append(rec)
            for ax,key,scale,title in zip(axes.flat,['rss_bytes','fds','threads','cycles'],[1e-6,1,1,1],['Whole-process RSS MB','Descriptors','Threads','Completed churn cycles']):
                ax.plot([num(t,'elapsed_s') for t in ts],[num(t,key)*scale for t in ts],label=label(r),color=palette(i%20));ax.set_title(title);ax.set_xlabel('Elapsed seconds')
        for ax in axes.flat:ax.set_ylim(bottom=0)
        fig.legend(*axes[0,0].get_legend_handles_labels(),loc='upper center',bbox_to_anchor=(.5,.94),ncol=4,fontsize=7);fig.suptitle(f'{block}: warm-up retained; native owner/I/O/helper thread populations included')
        doc.extend([save(fig,block+'-resources'),''])
    write(out/'RESOURCES.csv',resources)
    if resources:
        doc.extend(['| Block / policy | Requested s | Cycles | Starts = stops = drops | Sampled RSS first / last / max MB | FD last | Threads last |','|---|---:|---:|---:|---:|---:|---:|'])
        for r in resources:doc.append(f"| {r['dataset']} / {r['policy']} | {r['requested_seconds']} | {int(r['cycles']):,} | {int(r['started']):,} | {r['rss_bytes_first']/1e6:.1f} / {r['rss_bytes_last']/1e6:.1f} / {r['rss_bytes_max']/1e6:.1f} | {r['fds_last']:.0f} | {r['threads_last']:.0f} |")
    doc.extend(['','## Qualitative CPU profiles','','SIGPROF at 199 Hz, with libc/libgcc/pthread/vdso blocklisted because hardware perf is restricted. Leaf samples describe captured user-space worker stacks, not off-CPU wait, all process CPU or causal mutex cost. Inclusive category shares would overlap, so exact leaf counters are provided separately. Profiled processes are excluded from timed medians.','', '| Block / policy / placement | Captured executor samples | Other samples | Contended-mutex leaf samples | Flamegraph |','|---|---:|---:|---:|---|'])
    leaves=[]
    for r in raw:
        if r.get('phase')!='profile':continue
        path=a.data/'profiles'/f"{r['stem']}.stacks";counts=Counter();actor=other=0
        for line in path.read_text().splitlines():
            stack,count=line.rsplit(' ',1);count=int(count);thread=stack.split(';',1)[0]
            if not thread.startswith(('actor-worker','spawned-shard-','tokio-runtime-w','actor-reactor')):other+=count;continue
            actor+=count;counts[stack.rsplit(';',1)[-1]]+=count
        contended=sum(count for leaf,count in counts.items() if 'mutex' in leaf.lower() and 'contended' in leaf.lower())
        for leaf,count in counts.most_common():leaves.append(dict(dataset=r['dataset'],source=r['revision'],policy=label(r),layout=r['layout'],stem=r['stem'],leaf=leaf,count=count,executor_samples=actor))
        link=f"../../results/fede-overnight-2026-11-06/profiles/{r['stem']}.svg"
        doc.append(f"| {r['dataset']} / {label(r)} / {r['layout']} | {actor:,} | {other:,} | {contended:,} | [SVG]({link}) |")
    write(out/'PROFILE-LEAVES.csv',leaves)
    doc.extend(['','## Limits and next decisions','','Private prototypes are tools for locating costs. A faster synthetic graph does not establish better real applications or justify removing Tokio. Wake publication, cancellation, destructor ordering, helpers, timers/TCP, blocking behavior and graceful teardown are separate obligations. There is no CPU preemption for an ordinary Rust handler. Unbounded queues can still exhaust memory under overload.','', 'Repeat on an otherwise idle host and workload-representative payloads/I/O before promoting any design. Add hardware/off-CPU evidence only where host permissions permit; do not infer cache or kernel causes from user-space profiles. Archive negative results and failed correctness gates alongside improvements.',''])
    report='\n'.join(doc)+'\n';(out/'REPORT.md').write_text(report)
    body=markdown.markdown(report,extensions=['tables','fenced_code'])
    for f in figures:body=body.replace(f'src="graphs/{f.name}"',f'src="data:image/png;base64,{base64.b64encode(f.read_bytes()).decode()}"')
    style='body{max-width:1500px;margin:36px auto;padding:0 24px;background:#fcfcfa;color:#203443;font:16px/1.6 system-ui}h1,h2{line-height:1.2}h2{margin-top:2.5em;border-bottom:2px solid #d4e0e8;padding-bottom:.4em}img{width:100%;height:auto}table{display:block;overflow:auto;border-collapse:collapse;font-size:12px}th,td{padding:7px;border-bottom:1px solid #d4e0e8}th{background:#e9f0f3}a{color:#176a86}pre{overflow:auto;background:#eef2f3;padding:12px}'
    (out/'REPORT.html').write_text('<!doctype html><html lang="en"><meta charset="utf-8"><meta name="viewport" content="width=device-width"><title>Spawned overnight research</title><style>'+style+'</style><body>'+body+'</body></html>')
    print(f'Rendered {len(figures)} figures, {len(summary)} separate aggregate cells, {len(resources)} resource summaries')


if __name__=='__main__':
    main()
