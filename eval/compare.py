"""Compare fixed Loom evaluation JSON files using the pre-frozen thresholds.

Identical reports are an explicit BASELINE SELF-CHECK, not release acceptance:
known baseline failures remain visible and their absolute gates are not waived
for any different candidate. Quality failures exit 1; malformed inputs exit 2.
"""
import argparse
import hashlib
import json
from pathlib import Path
import sys

HERE = Path(__file__).resolve().parent


def load(path):
    return json.loads(Path(path).read_text(encoding='utf-8'))


def sha(path):
    return hashlib.sha256(Path(path).read_bytes()).hexdigest()


def render_value(value):
    if value is None:
        return 'N/A'
    if isinstance(value, float):
        return '%.4f' % value
    return str(value)


class Table:
    def __init__(self):
        self.rows = []

    def add(self, label, baseline, candidate, passed=True, note='', advisory=False):
        state = ('NOTE' if advisory else 'PASS') if passed else 'FAIL'
        self.rows.append({'metric':label,'baseline':baseline,'candidate':candidate,'status':state,'note':note})

    def show(self):
        print('| 检查项 | 基线 | 待测 | 判定 | 说明 |')
        print('|---|---:|---:|---|---|')
        for row in self.rows:
            cells = [row['metric'],render_value(row['baseline']),render_value(row['candidate']),row['status'],row['note']]
            print('| ' + ' | '.join(c.replace('|','/').replace('\n',' ') for c in cells) + ' |')

    @property
    def failures(self):
        return sum(r['status']=='FAIL' for r in self.rows)


def identity(table, base, candidate):
    table.add('结果 schema',base.get('schema'),candidate.get('schema'),base.get('schema')==candidate.get('schema')==1)
    table.add('固定问题套件',base.get('suite'),candidate.get('suite'),base.get('suite')==candidate.get('suite'))
    for field in ('questions_sha256','thresholds_sha256'):
        a,b = base.get('definitions',{}).get(field),candidate.get('definitions',{}).get(field)
        table.add('冻结输入 '+field,(a or '')[:12],(b or '')[:12],bool(a) and a==b)
    table.add('执行基础设施',base.get('execution_ok'),candidate.get('execution_ok'),base.get('execution_ok') is True and candidate.get('execution_ok') is True)
    names = set(base.get('samples',{})) | set(candidate.get('samples',{}))
    for name in sorted(names):
        a,b = base.get('samples',{}).get(name,{}),candidate.get('samples',{}).get(name,{})
        am,bm = a.get('manifest',{}).get('sha256'),b.get('manifest',{}).get('sha256')
        table.add('样本 '+name,a.get('status'),b.get('status'),a.get('status')==b.get('status')=='verified' and bool(am) and am==bm)


def check_relations(table, base, candidate, thresholds):
    a = base['sections']['relations']
    b = candidate['sections']['relations']
    table.add('关系标签完整',a.get('status'),b.get('status'),a.get('status')==b.get('status')=='complete',
              '缺标签必须先补跑基线，不能拿夹具代替真实标签')
    names = set(a.get('samples',{})) | set(b.get('samples',{}))
    for name in sorted(names):
        old,new = a.get('samples',{}).get(name,{}),b.get('samples',{}).get(name,{})
        if old.get('status')!='complete' or new.get('status')!='complete':
            table.add(name+' 标签可比较',old.get('status'),new.get('status'),False)
            continue
        table.add(name+' 标签冻结',old.get('labels_sha256','')[:12],new.get('labels_sha256','')[:12],
                  bool(old.get('labels_sha256')) and old.get('labels_sha256')==new.get('labels_sha256'))
        # 准确率看报出来的关系对不对，按它自己声称的确定程度分开算；没连上的算召回，不算误报。
        for claimed in ('exact','likely'):
            x,y = old.get('reported',{}).get(claimed),new.get('reported',{}).get(claimed)
            if not x or not y:
                table.add(name+' 报为 '+claimed+' 的准确率','缺','缺',False,'结果里没有 reported 一段；用当前脚本重跑')
                continue
            floor = thresholds['relations']['reported'][claimed]['min_precision']
            if not y['total']:
                table.add(name+' 报为 '+claimed+' 的准确率',x['precision'],'N/A',True,'标注的行上没有报出这一类；不虚构准确率',advisory=True)
                continue
            wrong = ','.join(e['id']+':'+e['reason'] for e in y['errors'])
            table.add(name+' 报为 '+claimed+' 的准确率',x['precision'],y['precision'],y['precision']>=floor,
                      '最低 %.0f%%；%d/%d %s'%(floor*100,y['correct'],y['total'],wrong))
        # 逐条标签：基线对的不能变错，也不能多出新的错。四类标签都算。
        old_correct = {r['id'] for r in old['labels'] if r['correct']}
        new_correct = {r['id'] for r in new['labels'] if r['correct']}
        lost = sorted(old_correct-new_correct)
        table.add(name+' 逐条无丢失',len(old_correct),len(new_correct),not lost,','.join(lost))
        old_errors = {r['id'] for r in old['labels'] if not r['correct']}
        new_errors = {r['id'] for r in new['labels'] if not r['correct']}
        added = sorted(new_errors-old_errors)
        table.add(name+' 无新增错误',len(old_errors),len(new_errors),not added,','.join(added))
        for cat in ('exact','likely','none','ambiguous'):
            x,y = old['categories'][cat],new['categories'][cat]
            table.add(name+' '+cat+' 标签',str(x['correct'])+'/'+str(x['total']),str(y['correct'])+'/'+str(y['total']),
                      x['total']==y['total'],'按标签类别计，仅供对照' if x['total'] else '没有这一类标签',advisory=x['total']==y['total'])
        table.add(name+' 应连上的关系里连上的比例',old['recall']['rate'],new['recall']['rate'],new['recall']['rate']>=old['recall']['rate'],
                  '%d/%d → %d/%d；不得低于基线'%(old['recall']['linked'],old['recall']['total'],new['recall']['linked'],new['recall']['total']))
        table.add(name+' 索引范围',old['index_report'].get('scope'),new['index_report'].get('scope'),
                  old['index_report'].get('scope')==new['index_report'].get('scope'))


def check_questions(table, base, candidate, thresholds):
    old = base['sections']['questions']
    new = candidate['sections']['questions']
    table.add('问题运行完整',old.get('status'),new.get('status'),old.get('status')==new.get('status')=='complete')
    table.add('错报不存在',old.get('false_negatives'),new.get('false_negatives'),
              type(new.get('false_negatives')) is int and new['false_negatives']<=thresholds['false_negatives_max'],'要求 0')
    left = {r['id']:r for r in old.get('results',[])}
    right = {r['id']:r for r in new.get('results',[])}
    table.add('固定问题集合',len(left),len(right),set(left)==set(right))
    memory_old=memory_new=0
    requires_contract2=any(r.get('report_contract',1)>=2 for r in right.values() if r.get('kind')=='structure')
    for key in sorted(set(left)|set(right)):
        a,b=left.get(key,{}),right.get(key,{})
        af,bf=a.get('facts',{}).get('status'),b.get('facts',{}).get('status')
        okay = bool(b) and b.get('status')!='error' and (af!='pass' or bf=='pass')
        if a.get('status')!='not_applicable' and b.get('status')=='not_applicable':
            okay=False
        table.add(key+' 事实',af,bf,okay,'保留旧版失败' if bf=='fail' and af=='fail' else '')
        ac,bc=a.get('contract',{}).get('status'),b.get('contract',{}).get('status')
        contract_ok=bc in ('pass','not_applicable') and not (ac=='pass' and bc!='pass')
        if b.get('kind')=='structure' and requires_contract2:
            contract_ok=contract_ok and b.get('report_contract',1)>=2 and bc=='pass'
        table.add(key+' 契约',ac,bc,contract_ok)
        if b.get('kind')=='memory' and key in ('M01','M03'):
            memory_old += sum(c['pass'] for c in a.get('facts',{}).get('checks',[]) if c.get('check')=='expected memory in top five')
            memory_new += sum(c['pass'] for c in b.get('facts',{}).get('checks',[]) if c.get('check')=='expected memory in top five')
        if b.get('kind')=='task':
            current=b.get('context',{})
            if current.get('status')=='not_applicable':
                table.add(key+' context','现有多次请求','N/A',True,'未提供 context，留待 U-3',advisory=True)
            else:
                legacy=b.get('evidence',{}).get('items',{})
                combined=current.get('evidence',{}).get('items',{})
                lost=[k for k,v in legacy.items() if v.get('pass') and not combined.get(k,{}).get('pass')]
                table.add(key+' context 正确依据',b.get('evidence',{}).get('correct'),current.get('evidence',{}).get('correct'),not lost and current.get('facts',{}).get('status')=='pass','丢失: '+','.join(lost) if lost else '')
                cheaper = current.get('delivery_requests',10**9)<b.get('delivery_requests',0) or current.get('delivery_bytes',10**9)<b.get('delivery_bytes',0)
                table.add(key+' context 成本',str(b.get('delivery_requests'))+'/'+str(b.get('delivery_bytes')),
                          str(current.get('delivery_requests'))+'/'+str(current.get('delivery_bytes')),cheaper,'请求数/UTF-8 字节；同一待测版本内比较')
                table.add(key+' context 必要提示预算',True,current.get('within_budget'),current.get('within_budget') is True)
    table.add('新增记忆问题前五命中',memory_old,memory_new,memory_new>=memory_old)


def resource_section(report, override, table, label):
    if not override:
        return report['sections']['resources']
    other=load(override)
    matches=(other.get('executable',{}).get('sha256')==report.get('executable',{}).get('sha256') and
             other.get('definitions')==report.get('definitions') and
             {k:v.get('manifest') for k,v in other.get('samples',{}).items()}=={k:v.get('manifest') for k,v in report.get('samples',{}).items()})
    table.add(label+' 同场资源覆盖文件','同一二进制和输入',matches,matches)
    return other['sections']['resources']


def check_resources(table, base, candidate, thresholds):
    if base.get('status')=='not_run' or candidate.get('status')=='not_run':
        table.add('资源','未同场测量','未同场测量',True,'需要同场复测；不据此宣称资源验收通过',advisory=True)
        return
    table.add('资源执行',base.get('status'),candidate.get('status'),base.get('status')==candidate.get('status')=='complete')
    paired=bool(base.get('pair_id')) and base.get('pair_id')==candidate.get('pair_id') and base.get('interleaved') and candidate.get('interleaved')
    table.add('同场逐轮轮换',base.get('pair_id'),candidate.get('pair_id'),True,
              '正式同场资源结果' if paired else '需要同场复测；冻结基线的单版本试跑只证明脚本可用',advisory=not paired)
    for name in sorted(set(base.get('samples',{}))|set(candidate.get('samples',{}))):
        a,b=base.get('samples',{}).get(name,{}),candidate.get('samples',{}).get(name,{})
        for op in ('full','unchanged','one_file','explain','impact'):
            x,y=a.get('operations',{}).get(op,{}),b.get('operations',{}).get(op,{})
            xm,ym=x.get('ms',{}),y.get('ms',{})
            count_ok=min(xm.get('rounds',0),ym.get('rounds',0))>=thresholds['resources']['min_rounds']
            table.add(name+' '+op+' 轮数',xm.get('rounds'),ym.get('rounds'),count_ok)
            xv,yv=xm.get('median'),ym.get('median')
            if xv is None or yv is None:
                table.add(name+' '+op+' 毫秒中位数',xv,yv,False)
            else:
                increase=(yv/xv-1) if xv else float('inf')
                note=('需要说明：+%.1f%%'%(increase*100)) if increase>thresholds['resources']['median_increase_explain_above'] else ('%+.1f%%'%(increase*100))
                if not paired:
                    note+='；非同场仅供参考'
                table.add(name+' '+op+' 毫秒中位数',xv,yv,True,note,advisory=True)
            xp,yp=x.get('peak_working_set_bytes',{}),y.get('peak_working_set_bytes',{})
            table.add(name+' '+op+' 峰值工作集最大字节',xp.get('max'),yp.get('max'),
                      xp.get('observations')==xm.get('rounds') and yp.get('observations')==ym.get('rounds'))
    for label,section in [('baseline',base),('candidate',candidate)]:
        idle=section.get('idle',[])
        table.add(label+' serve 空闲样本数',3,len(idle),len(idle)==3)
        for row in idle:
            value=row.get('working_set_bytes')
            table.add(label+' '+row.get('sample','?')+' 空闲工作集',None,value,
                      value is not None and row.get('idle_seconds',0)>=thresholds['resources']['idle_seconds'],
                      '超过 100 MiB 设计目标' if value and value>=thresholds['resources']['idle_working_set_goal_bytes'] else '',advisory=value is not None)


def self_check(table, report):
    table.add('冻结基线自校验','相同完整结果','相同完整结果',True,
              '仅检验可复读/对照链路；不是新版验收，以下保留原始失败',advisory=True)
    q=report['sections']['questions']
    failures=[r['id'] for r in q.get('results',[]) if r.get('facts',{}).get('status') in ('fail','error')]
    table.add('基线事实层失败',len(failures),len(failures),True,','.join(failures),advisory=True)
    table.add('基线错报不存在',q.get('false_negatives'),q.get('false_negatives'),True,'正式新版本比较仍要求 0',advisory=True)
    relations=report['sections']['relations']
    table.add('基线标签状态',relations.get('status'),relations.get('status'),True,'pending 时不代表关系指标已完成',advisory=True)
    table.add('执行基础设施',True,report.get('execution_ok'),report.get('execution_ok') is True)
    for name, sample in relations.get('samples',{}).items():
        if sample.get('status')=='complete':
            for cat,row in sample['categories'].items():
                table.add(name+' '+cat,row.get('correct'),row.get('correct'),True,'总数 '+str(row.get('total')),advisory=True)
    for row in q.get('results',[]):
        table.add(row['id']+' 结果/字节/请求',row.get('facts',{}).get('status'),
                  str(row.get('answer_bytes'))+'/'+str(row.get('requests')),True,advisory=True)


def main():
    parser=argparse.ArgumentParser(description=__doc__)
    parser.add_argument('baseline')
    parser.add_argument('candidate')
    parser.add_argument('--thresholds',default=str(HERE/'thresholds.json'))
    parser.add_argument('--baseline-resources')
    parser.add_argument('--candidate-resources')
    parser.add_argument('--out',help='Optional machine-readable comparison JSON')
    args=parser.parse_args()
    if hasattr(sys.stdout,'reconfigure'):
        sys.stdout.reconfigure(encoding='utf-8')
    if not sys.flags.isolated:
        parser.error('Use python -I eval/compare.py')
    try:
        base,candidate=load(args.baseline),load(args.candidate)
        thresholds=load(args.thresholds)
        table=Table()
        identity(table,base,candidate)
        threshold_sha=sha(args.thresholds)
        table.add('使用冻结门槛',base.get('definitions',{}).get('thresholds_sha256','')[:12],threshold_sha[:12],
                  base.get('definitions',{}).get('thresholds_sha256')==threshold_sha and candidate.get('definitions',{}).get('thresholds_sha256')==threshold_sha)
        same=(base==candidate and not args.baseline_resources and not args.candidate_resources)
        if same:
            self_check(table,base)
        else:
            check_relations(table,base,candidate,thresholds)
            check_questions(table,base,candidate,thresholds)
            a=resource_section(base,args.baseline_resources,table,'基线')
            b=resource_section(candidate,args.candidate_resources,table,'待测')
            check_resources(table,a,b,thresholds)
        table.show()
        print(('BASELINE SELF-CHECK' if same else 'QUALITY COMPARISON') + ': ' + ('PASS' if not table.failures else 'FAIL') + ' (%d failed checks)'%table.failures)
        print('预算超限、凭据、项目隔离、并发混代次由 README 指定的仓库测试验收；本命令不把它们算成已跑。')
        if args.out:
            Path(args.out).write_text(json.dumps({'schema':1,'self_check':same,'pass':not table.failures,'rows':table.rows},ensure_ascii=False,indent=2)+'\n',encoding='utf-8',newline='\n')
        return 1 if table.failures else 0
    except Exception as exc:
        print('ERROR: '+str(exc),file=sys.stderr)
        return 2


if __name__=='__main__':
    raise SystemExit(main())
