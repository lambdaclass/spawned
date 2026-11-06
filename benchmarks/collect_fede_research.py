#!/usr/bin/env python3
"""Archive original runs and assemble a lossless combined view for the report."""
import argparse
import csv
import hashlib
import json
from pathlib import Path
import shutil
import tarfile


def digest(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()


def read_rows(path):
    with path.open() as file:
        return list(csv.DictReader(file))


def main():
    parser=argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--primary',type=Path,required=True)
    parser.add_argument('--confirmation',type=Path,required=True)
    parser.add_argument('--source-bundle',type=Path,required=True)
    parser.add_argument('--controller',type=Path,required=True)
    parser.add_argument('--output',type=Path,required=True)
    args=parser.parse_args();out=args.output;out.mkdir(parents=True,exist_ok=False)
    combined=[];inputs={};base=None
    for name,root in [('main',args.primary),('confirmation',args.confirmation)]:
        meta=json.loads((root/'metadata.json').read_text())
        assert meta.get('finished_utc') and not meta['dirty']
        if base is None:base=meta
        assert meta['revision']==base['revision'] and meta['binaries']==base['binaries']
        assert all(meta[k]==base[k] for k in ['actor_cpus','driver_cpus','collector_cpus','environment'])
        dest=out/'runs'/name;dest.mkdir(parents=True)
        for file in ['raw.csv','metadata.json','build.log']:shutil.copyfile(root/file,dest/file)
        original=read_rows(root/'raw.csv')
        for row in original:
            assert row['status']=='ok'
            if row.get('succeeded'):
                assert int(row['succeeded'])+int(row['timeouts'])==int(row['messages'])
            derived=dict(row,campaign=name,combined_index=len(combined),stem=f'{name}/{row["stem"]}')
            combined.append(derived)
        for directory in ['series','threads','profiles']:
            shutil.copytree(root/directory,out/directory/name)
        with tarfile.open(out/f'diagnostics-{name}.tar.gz','w:gz') as archive:
            archive.add(root/'logs',arcname='logs')
        inputs[name]=dict(samples=len(original),metadata_sha256=digest(root/'metadata.json'),
            raw_sha256=digest(root/'raw.csv'),revision=meta['revision'],binaries=meta['binaries'])
        if name=='confirmation':
            base['hot_reference']=meta.get('hot_reference')
            base['confirmation_controller_sha256']=meta['controller_sha256']
            base['finished_utc']=meta['finished_utc']
    fields=list(dict.fromkeys(k for r in combined for k in r))
    with (out/'raw.csv').open('w') as file:
        writer=csv.DictWriter(file,fields,lineterminator='\n');writer.writeheader();writer.writerows(combined)
    base['inputs']=inputs
    base['derivation']='Original CSVs/metadata retained in runs/. Combined CSV adds campaign/combined_index and prefixes stem for artifact paths; numeric fields and config_json unchanged.'
    base['samples']=len(combined)
    (out/'metadata.json').write_text(json.dumps(base,indent=2)+'\n')
    shutil.copyfile(args.source_bundle,out/'source.bundle')
    assert digest(args.controller)==base['confirmation_controller_sha256'], 'controller must match measured orchestration'
    shutil.copyfile(args.controller,out/'confirmation-controller.py')
    manifest={str(p.relative_to(out)):digest(p) for p in sorted(out.rglob('*')) if p.is_file()}
    (out/'sha256.json').write_text(json.dumps(manifest,indent=2)+'\n')
    print(f'Archived {len(combined)} successful processes; {len(manifest)} hashed artifacts')


if __name__=='__main__':
    main()
