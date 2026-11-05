#!/usr/bin/env python3
"""Regenerate non-price TPU facts. Only authenticated GETs; no resource/IAM writes.

After gpu setup: python3 tools/update_tpu_facts.py --project PROJECT
Offline regeneration: --input captured_facts.json (same raw metadata schema).
Dollar amounts, account details and OAuth credentials never enter the snapshot.
"""
import argparse, concurrent.futures, csv, datetime, json, os, pathlib, re, subprocess, tempfile, urllib.parse, urllib.request
ROOT=pathlib.Path(__file__).resolve().parent.parent

class NoRedirect(urllib.request.HTTPRedirectHandler):
    def redirect_request(self,*args,**kwargs): raise RuntimeError('API redirect refused')

def generation(text):
    text=re.sub(r'[^a-z0-9]','',text.lower())
    if 'v6e' in text or 'trillium' in text:return 'v6e'
    if 'v5enterprise' in text or 'v5p' in text:return 'v5p'
    if 'v5lite' in text or 'v5e' in text:return 'v5e'
    for gen in ['v4','v3','v2']:
        if gen in text:return gen
    return None

def generate(raw,regions):
    zones=[]
    for zone,types in sorted(raw['zones'].items()):
        names=sorted(set(types))
        if names:zones.append({'name':zone,'types':names})
    services=[{'name':s['name'],'display_name':s['displayName']} for s in raw['services'] if s['displayName'] in ['Cloud TPU','Compute Engine']]
    skus=[]
    for service,items in raw['skus'].items():
        for s in items:
            desc=s.get('description','');cat=s.get('category',{})
            if 'tpu' not in (desc+' '+cat.get('resourceGroup','')).lower():continue
            info=s.get('pricingInfo',[{}])[-1].get('pricingExpression',{})
            skus.append({'id':s['skuId'],'service':service,'description':desc,'category':cat,'regions':s.get('serviceRegions',[]),'generation':generation(desc+' '+cat.get('resourceGroup','')),
                         'unit':{k:info[k] for k in ['usageUnit','usageUnitDescription','baseUnit','baseUnitDescription','baseUnitConversionFactor','displayQuantity'] if k in info}})
    region_names=set(regions)
    region_names.update(z['name'].rsplit('-',1)[0] for z in zones)
    region_names.update(r for s in skus for r in s['regions'] if re.fullmatch(r'[a-z]+-[a-z]+[0-9]+',r))
    return {'schema':1,'generated_at':raw.get('generated_at',datetime.datetime.now(datetime.timezone.utc).isoformat()),
            'sources':['Cloud TPU v1 locations + v2 acceleratorTypes','Cloud Billing v1 services/skus','https://cloud.google.com/about/locations'],
            'types':sorted({t for z in zones for t in z['types']}),'generations':sorted({generation(t) for z in zones for t in z['types'] if generation(t)} | {s['generation'] for s in skus if s['generation']}),
            'zones':zones,'regions':[{'name':r,**regions.get(r,{})} for r in sorted(region_names)],'services':sorted(services,key=lambda s:s['name']),'skus':sorted(skus,key=lambda s:(s['service'],s['id']))}

def live(project):
    config=pathlib.Path(os.environ.get('XDG_CONFIG_HOME') or pathlib.Path.home()/'.config')/'gpu/config.json'
    cfg=json.loads(config.read_text()).get('gcp',{})
    setup=cfg.get('setup',{})
    if cfg.get('project')!=project or setup.get('project')!=project or setup.get('version')!=1 or setup.get('removing'):raise SystemExit('Run gpu setup for this project first.')
    token=subprocess.run(['gcloud','auth','print-access-token','--project='+project,'--quiet'],stdin=subprocess.DEVNULL,capture_output=True,check=True,text=True,timeout=30).stdout.strip()
    opener=urllib.request.build_opener(urllib.request.ProxyHandler({}),NoRedirect())
    def get(url):
        if not url.startswith(('https://tpu.googleapis.com/v1/','https://tpu.googleapis.com/v2/','https://cloudbilling.googleapis.com/v1/')):raise RuntimeError('Unexpected API host')
        request=urllib.request.Request(url,headers={'Authorization':'Bearer '+token,'x-goog-user-project':project})
        with opener.open(request,timeout=30) as response:return json.load(response)
    def pages(base,key,size=5000):
        rows=[];next_token=''
        for _ in range(100):
            url=base+('&' if '?' in base else '?')+urllib.parse.urlencode({'pageSize':size,'pageToken':next_token})
            data=get(url);rows.extend(data.get(key,[]));next_token=data.get('nextPageToken','')
            if not next_token:return rows
        raise RuntimeError('Pagination limit reached')
    locations=pages(f'https://tpu.googleapis.com/v1/projects/{project}/locations','locations',1000)
    def types(item):
        zone=item.get('locationId') or item['name'].rsplit('/',1)[-1]
        values=pages(f'https://tpu.googleapis.com/v2/projects/{project}/locations/{zone}/acceleratorTypes','acceleratorTypes',1000)
        return zone,[v.get('type') or v['name'].rsplit('/',1)[-1] for v in values]
    with concurrent.futures.ThreadPoolExecutor(max_workers=8) as pool:zones=dict(pool.map(types,locations))
    services=pages('https://cloudbilling.googleapis.com/v1/services','services')
    chosen=[s for s in services if s['displayName'] in ['Cloud TPU','Compute Engine']]
    fields='skus(skuId,description,category,serviceRegions,pricingInfo(pricingExpression(usageUnit,usageUnitDescription,baseUnit,baseUnitDescription,baseUnitConversionFactor,displayQuantity))),nextPageToken'
    def skus(service):
        return service['name'],pages('https://cloudbilling.googleapis.com/v1/'+service['name']+'/skus?'+urllib.parse.urlencode({'currencyCode':'USD','fields':fields}),'skus')
    with concurrent.futures.ThreadPoolExecutor(max_workers=2) as pool:metadata=dict(pool.map(skus,chosen))
    return {'zones':zones,'services':chosen,'skus':metadata}

def main():
    parser=argparse.ArgumentParser(description=__doc__);parser.add_argument('--project');parser.add_argument('--input',type=pathlib.Path);parser.add_argument('--output',type=pathlib.Path,default=ROOT/'src/data/tpu_facts.json');parser.add_argument('--regions',type=pathlib.Path,default=ROOT/'tools/gcp_region_locations.csv')
    args=parser.parse_args()
    if not args.input and (not args.project or not re.fullmatch(r'[a-z][a-z0-9-]{4,61}[a-z0-9]',args.project)):parser.error('Use --project PROJECT or --input FILE')
    regions={}
    with args.regions.open() as f:
        for row in csv.DictReader(line for line in f if not line.startswith('#')):regions[row['region']]={'latitude':float(row['latitude']),'longitude':float(row['longitude'])}
    raw=json.loads(args.input.read_text()) if args.input else live(args.project)
    result=generate(raw,regions);args.output.parent.mkdir(parents=True,exist_ok=True)
    with tempfile.NamedTemporaryFile(mode='w',dir=args.output.parent,delete=False) as f:json.dump(result,f,indent=2,sort_keys=True);f.write('\n');temporary=f.name
    pathlib.Path(temporary).replace(args.output)
    print(f'Wrote {len(result["types"])} types, {len(result["zones"])} zones, {len(result["skus"])} SKU identifiers; no prices.')
if __name__=='__main__':main()
