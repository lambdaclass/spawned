#!/usr/bin/env python3
"""Long graph confirmation and baseline-only near-saturation grid refinement.

Run after the primary campaign, against its unchanged clean source checkout.
The controller itself may live outside that checkout; its exact hash is recorded.
"""
import argparse
import csv
import hashlib
import importlib.util
import json
from pathlib import Path
import random
import statistics


def read_rows(path):
    with path.open() as file:
        return list(csv.DictReader(file))


def hot_reference(primary, workers):
    """Estimate matched-mix capacity from hot backlog, not growing cold traffic."""
    estimates = []
    evidence = []
    for row in read_rows(primary / 'raw.csv'):
        if row['phase'] != 'pilot' or int(row['workers']) != workers or row.get('driver_valid') != 'True':
            continue
        config = json.loads(row['config_json'])
        duration = int(config['messages']) / int(config['rate'])
        series = [r for r in read_rows(primary/'series'/f'{row["stem"]}.csv')
                  if duration*.25 <= float(r['elapsed_ms'])/1000 <= duration*.9]
        if len(series) < 2:
            continue
        first, last = series[0], series[-1]
        dt = (float(last['elapsed_ms'])-float(first['elapsed_ms']))/1000
        admitted = (int(last['submitted'])-int(first['submitted']))/dt
        hot_growth = (int(last['hot_queued'])-int(first['hot_queued']))/dt
        cold_growth = (int(last['cold_queued'])-int(first['cold_queued']))/dt
        record = dict(pilot_index=row['index'], admitted_per_s=admitted,
                      hot_queue_growth_per_s=hot_growth, cold_queue_growth_per_s=cold_growth)
        # Routing is deterministically 80/20; finite-window edge error is only a
        # few IDs per producer. Started/completed differ by at most active actors.
        if hot_growth > max(1000, admitted*.01) and cold_growth <= max(1000, admitted*.01):
            estimate = (admitted*.8-hot_growth)/.8
            if estimate > 0:
                estimates.append(estimate)
                record['hot_corrected_reference_per_s'] = estimate
        evidence.append(record)
    return statistics.median(estimates) if estimates else None, evidence


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--source', type=Path, required=True)
    parser.add_argument('--primary', type=Path, required=True)
    parser.add_argument('--output', type=Path, required=True)
    args = parser.parse_args()
    primary = json.loads((args.primary/'metadata.json').read_text())
    assert primary.get('finished_utc'), 'finish primary measurement before building/running confirmation'
    spec = importlib.util.spec_from_file_location('fede_research', args.source/'benchmarks/fede_research.py')
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    config = argparse.Namespace(output=args.output, workers=[4], variants=list(module.VARIANTS),
        actor_cpus=primary['actor_cpus'], driver_cpus=primary['driver_cpus'],
        collector_cpus=primary['collector_cpus'], seed=182, duration=2., timeout=120., smoke=False)
    campaign = module.Campaign(config)
    assert campaign.meta['revision'] == primary['revision'] and not campaign.meta['dirty']
    assert campaign.meta['binaries'] == primary['binaries'], 'confirmation must use identical benchmark executables'
    campaign.meta['calibration'] = primary['calibration']
    campaign.meta['controller_sha256'] = hashlib.sha256(Path(__file__).read_bytes()).hexdigest()
    campaign.meta['primary_metadata_sha256'] = hashlib.sha256((args.primary/'metadata.json').read_bytes()).hexdigest()
    campaign.meta['purpose'] = 'Longer graph windows; refine near-capacity points using only baseline pilot hot backlog, never candidate results'
    reference, evidence = hot_reference(args.primary, 4)
    campaign.meta['hot_reference'] = dict(workers=4, reference_per_s=reference, evidence=evidence,
        interpretation='approximate matched-mix reference inferred from baseline hot backlog with valid admission schedule and no growing cold backlog; not a production capacity')
    campaign.save()
    graph_jobs = [(v,t,l,r) for v in module.VARIANTS for t in ['chain','fanout']
                  for l in ['same','cross'] for r in range(5)]
    random.Random(182).shuffle(graph_jobs)
    for variant, topology, layout, repeat in graph_jobs:
        campaign.sample('graph-long',variant,4,dict(mode='graph',topology=topology,layout=layout,
            actors=1024,width=8,producers=256,messages=1_048_576,iterations=100),repeat)
    if reference:
        primary_rates = sorted({int(json.loads(r['config_json'])['rate']) for r in read_rows(args.primary/'raw.csv')
            if r['phase']=='offered' and r['workers']=='4' and int(json.loads(r['config_json']).get('burst',1))==1})
        # This selection rule is based on baseline pilots and was specified before
        # analyzing candidate load outcomes. Every policy gets the same new rates.
        if not any(.85 <= rate/reference <= 1.15 for rate in primary_rates):
            jobs = [(v,l,ratio,r) for v in module.VARIANTS for l in ['spread','concentrated']
                    for ratio in [.9,1.05] for r in range(3)]
            random.Random(183).shuffle(jobs)
            for variant, layout, ratio, repeat in jobs:
                rate=round(reference*ratio)
                campaign.sample('offered',variant,4,dict(mode='offered',layout=layout,actors=128,
                    producers=4,messages=min(round(rate*2),4_000_000),rate=rate,iterations=100,
                    **{'hot-iterations':5000}),repeat)
    campaign.meta['finished_utc'] = module.time.strftime('%Y-%m-%dT%H:%M:%SZ',module.time.gmtime())
    campaign.save()


if __name__ == '__main__':
    main()
