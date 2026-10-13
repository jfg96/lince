#!/usr/bin/env python3
"""Interleaved FSearch/full-path vs both lince versions, exact set validation.
No FSearch engine modifications: only benchmark flags and post-timing hashes.
"""
import argparse, hashlib, json, os
from pathlib import Path
import statistics, subprocess, sys, tempfile, time
sys.path.insert(0, str(Path(__file__).resolve().parents[1] / 'tests'))
import bench_compare as ix
ix.EXCLUDES = ['.git','node_modules','.cache','__pycache__','.npm','.nvm','.cargo',
               '.rustup','.gradle','.venv','venv','.local/share/Trash','.local/share/flatpak']
QUERIES = ['h','x','q','z','hy','rs','js','md','qt','zz']

def cases(root):
    for q in QUERIES:
        if q in root.lower():
            continue  # A full-path search would match the root prefix itself.
        for kind in ['a','d','f']:
            yield kind, q, {'query':q,'hidden':True,'kinds': ['d','f'] if kind=='a' else [kind], 'max_results':2000}

class FSearch:
    def __init__(self, binary, root, work, repeats, hashing=False):
        self.repeats = repeats
        self.log = open(work / ('hash.log' if hashing else 'timing.log'), 'w+')
        env = os.environ.copy()
        env['FSBENCH_FULL_PATH'] = '1'
        env.pop('G_MESSAGES_DEBUG', None)
        if hashing: env['FSBENCH_HASH'] = '1'
        else: env.pop('FSBENCH_HASH', None)
        self.hashing = hashing
        self.proc = subprocess.Popen([binary,root,str(work/'fs.db'),str(repeats)],
                                     stdin=subprocess.PIPE,stdout=subprocess.PIPE,stderr=self.log,text=True,env=env)
    def ask(self, kind, query):
        self.proc.stdin.write(kind+'\t'+query+'\n'); self.proc.stdin.flush()
        line = self.proc.stdout.readline()
        if not line:
            self.log.seek(0); raise RuntimeError(self.log.read())
        f = line.rstrip('\n').split('\t')
        assert f[0]==kind and f[1]==query, line
        out={'wall_ms':float(f[3]),'cpu_ms':float(f[4]),'dirs':int(f[5]),'files':int(f[6])}
        if self.hashing:
            f = self.proc.stdout.readline().strip().split('\t')
            if f[0]!='HASH':
                self.log.seek(0); raise RuntimeError(self.log.read())
            out['sha256']=f[1]
        return out
    def close(self):
        self.proc.stdin.close()
        self.proc.wait(timeout=30)
        self.log.seek(0); self.text=self.log.read(); self.log.close()
        if self.proc.returncode != 0: raise RuntimeError(self.text)


def digest(r):
    h=hashlib.sha256()
    for b in sorted(p.encode('utf-8') for p in r['dirs']+r['files']): h.update(b+b'\0')
    return h.hexdigest()


def main():
    p=argparse.ArgumentParser()
    p.add_argument('root');p.add_argument('fsbench');p.add_argument('baseline');p.add_argument('candidate');p.add_argument('output')
    p.add_argument('--repeats',type=int,default=15)
    a=p.parse_args(); root=str(Path(a.root).resolve())
    if a.repeats < 1: p.error('--repeats must be positive')
    rows=[]; all_cases=list(cases(root))
    if not all_cases: p.error('all query terms occur in the root path')
    with tempfile.TemporaryDirectory(prefix='lince-fs-control-') as tmp:
        work=Path(tmp)
        seed=ix.Daemon(a.baseline,root,work,'seed');seed.close()
        daemons=[];fs=None;fh=None
        try:
            for name,b in [('baseline',a.baseline),('candidate',a.candidate)]:
                daemons.append(ix.Daemon(b,root,work,name,seed.snapshot))
            fs=FSearch(a.fsbench,root,work,a.repeats)
            for kind,q,req in all_cases:
                samples=[[],[]]; cpu=[0.,0.]; fs_samples=[]
                for d in daemons:
                    for _ in range(3): d.ask(req)
                for order in [[0,1,2],[1,2,0],[2,0,1],[2,1,0]]:
                    for k in order:
                        if k==2:
                            fs_samples.append(fs.ask(kind,q))
                        else:
                            d=daemons[k]; c=d.cpu()
                            for _ in range(a.repeats):
                                wall,r=d.ask(req)
                                samples[k].append([wall,r['engine_us']/1000,*r['phases_us']])
                            cpu[k]+=d.cpu()-c
                row={'query':q,'kind':kind,'request':req,
                     'fsearch':{key:statistics.median(f[key] for f in fs_samples) for key in ['wall_ms','cpu_ms']},
                     'fsearch_counts':[fs_samples[-1]['dirs'],fs_samples[-1]['files']]}
                for i,name in enumerate(['baseline','candidate']):
                    row[name]={'wall_ms':statistics.median(v[0] for v in samples[i]),
                               'engine_ms':statistics.median(v[1] for v in samples[i]),
                               'cpu_ms':cpu[i]/len(samples[i]),'samples':samples[i]}
                rows.append(row)
                print(f"{q:3} {kind} FS {row['fsearch']['wall_ms']:7.3f} old {row['baseline']['wall_ms']:7.3f} new {row['candidate']['wall_ms']:7.3f}",flush=True)
            statuses=[d.ask({'op':'status'})[1] for d in daemons]
            fs.close();timing_log=fs.text;fs=None
            Path(a.output+'.timing.json').write_text(json.dumps({'root':root,'rows':rows,'status':statuses,'fsearch_log':timing_log},indent=2)+'\n')
            # Separate, untimed validation pass: same candidates, regardless of
            # the engines' different result ordering and display caps.
            fh=FSearch(a.fsbench,root,work,1,hashing=True)
            for row in rows:
                f=fh.ask(row['kind'],row['query'])
                req={**row['request'],'max_results':10**8,'select':'first'}
                rs=[d.ask(req)[1] for d in daemons]
                assert digest(rs[0])==digest(rs[1]), ('lince mismatch', req)
                row['lince_counts']=[rs[1]['matched_dirs'],rs[1]['matched_files']]
                row['fsearch_sha256']=f['sha256'];row['lince_sha256']=digest(rs[1])
                row['same_candidates']=row['fsearch_counts']==row['lince_counts'] and f['sha256']==row['lince_sha256']
                print('parity',row['query'],row['kind'],row['same_candidates'],row['fsearch_counts'],row['lince_counts'],flush=True)
            fh.close();hash_log=fh.text;fh=None
        finally:
            for f in [fs,fh]:
                if f is not None:
                    f.proc.terminate();f.proc.wait()
            for d in daemons:d.close()
    equal=[r for r in rows if r['same_candidates']]
    summary={'verified_cases':len(equal),'total_cases':len(rows),'status':statuses}
    for name in ['fsearch','baseline','candidate']:
        summary[name]={k:statistics.median(r[name][k] for r in equal) for k in ['wall_ms','cpu_ms']} if equal else {}
    out={'root':root,'fsearch_commit':'6d119c034410516cfb2104108fc96d1c29a7b222',
         'fsearch_build':'release, LTO, unmodified engine',
         'binary_sha256':{n:hashlib.sha256(Path(getattr(a,n)).read_bytes()).hexdigest() for n in ['fsbench','baseline','candidate']},
         'repeats_per_case':a.repeats*4,'timing_log':timing_log,'summary':summary,'rows':rows,'validation_log':hash_log}
    Path(a.output).write_text(json.dumps(out,indent=2)+'\n')
    print(json.dumps(summary,indent=2),flush=True)

if __name__=='__main__':main()
