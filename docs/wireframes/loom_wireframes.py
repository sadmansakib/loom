#!/usr/bin/env python3
"""Loom TUI wireframes generator. Python 3 only, no dependencies.

  python3 loom_wireframes.py [out_dir]     (default: current directory)

Writes into out_dir:
  index.html       all screens in one scrollable page (open in any browser)
  *.dc.html        one file per screen (for the Claude canvas)
  canvas.json      canvas layout (kept if it already exists)
  DESIGN.md        palette, glyphs, layout and command rules
Edit the screen definitions below and re-run. Colors live in CSS and PAL.
"""
import sys,os
GAL=[]
OUT=sys.argv[1] if len(sys.argv)>1 else "."
os.makedirs(OUT,exist_ok=True); os.chdir(OUT)
CSS="""body{margin:0}.t{width:1200px;height:760px;background:#12100E;color:#E9E3D7;font:13px/20px 'JetBrains Mono',ui-monospace,Menlo,monospace;display:flex;flex-direction:column;box-sizing:border-box;position:relative;overflow:hidden}.hd{height:36px;padding:0 16px;display:flex;align-items:center;justify-content:space-between;border-bottom:1px solid #2a241c;background:#161210}.m{display:flex;flex:1;min-height:0}.th{flex:1;padding:14px 16px}.sb{width:300px;border-left:1px solid #2a241c;padding:8px 16px}.cp{margin:0 16px 8px;border:1px solid #E3A857;padding:8px 12px;color:#9A9285}.ft{padding:6px 16px;display:flex;justify-content:space-between;color:#9A9285;font-size:12px}.k{border:1px solid #5A4A2C;padding:0 5px;margin-right:4px;color:#E9E3D7;font-size:11px}.a{color:#E3A857}.s{color:#86C19F}.c{color:#EE8272}.v{color:#A99BD6;font-style:italic}.b{color:#8DB4E8}.g{color:#6FC4C4}.d{color:#9A9285}.w{font-weight:700}.lb{color:#9A9285;letter-spacing:.12em;font-size:11px;margin:14px 0 4px}.bx{border:1px solid #5A4A2C;background:#1a1612;padding:10px 14px;margin:10px 0}.bx.h{border-color:#E3A857}.r{display:flex;justify-content:space-between}.df{background:#1a1612;border:1px solid #2a241c;padding:6px 12px;margin:6px 0 6px 18px}.ov{position:absolute;inset:0;background:rgba(10,9,8,.82)}.md{position:absolute;left:110px;top:70px;width:980px;background:#14110e;border:1px solid #E3A857;padding:0 0 0}.md>div{padding:0 20px}.rw{padding:6px 12px;border:1px solid transparent;margin:3px 0}.rw.on{border-color:#E3A857;background:#1c1710}.bar{height:6px;background:#2a241c;margin:4px 0}.bar i{display:block;height:6px;background:#E3A857}"""
def page(title,body):
    return f"""<!doctype html><html lang="en"><head><meta charset="utf-8"><title>{title}</title><script src="./support.js"></script></head><body><x-dc><helmet><link rel="stylesheet" href="https://fonts.googleapis.com/css2?family=JetBrains+Mono:wght@400;700&display=swap"><style>{CSS}</style></helmet>{body}</x-dc><script type="text/x-dc" data-dc-script data-props='{{"$preview":{{"width":1200,"height":760}}}}'>
class Component extends DCLogic {{ renderVals() {{ return {{}}; }} }}
</script></body></html>"""
def hd(chip,extra=""): return f'<div class="hd"><span><span class="a">◆ <b>loom</b></span> <span class="d"> ~/work/loom · session 0194a7e2</span></span><span>{chip} <span class="b">deepseek-flash</span> <span class="d">effort</span> <b>high</b></span></div>'
def ft(l,r): return f'<div class="ft"><span>{l}</span><span>{r}</span></div>'
def k(a,b): return f'<span class="k">{a}</span>{b} &nbsp;'
def lb(x): return f'<div class="lb">{x}</div>'
def r(a,b): return f'<div class="r"><span>{a}</span><span>{b}</span></div>'

# 1 approval
th1=f"""<div class="w">❯ Add a jitter test for RetryPolicy and make it pass.</div>
<div class="d"><span class="a">●</span> turn 3 <span class="s">✓</span> 2 calls · 6,120 in · 240 out</div>
<div class="d"><span class="a">●</span> turn 4 <span class="a">⠇ waiting for you</span></div>
<div style="border-left:1px solid #8C6B33;margin-left:3px;padding-left:12px">
<div>I'll add the test next to the existing backoff tests.</div><div class="d">1 write call · needs approval</div>
<div>├─ <span class="a">◇</span> <b class="b">write_file</b> crates/loom-core/src/retry.rs <span class="d">+18 −0 · awaiting</span></div>
<div class="df"><div class="d">@@ retry.rs · 141</div><div class="s">+ #[test]</div><div class="s">+ fn jitter_stays_within_25_percent() {{ … }}</div><div class="d">… 16 more lines · ^O expand</div></div>
<div>└─ <span class="d">○</span> <b class="b">run_tests</b> <span class="d">queued · runs after approval</span></div></div>
<div class="bx h"><div class="a w">Allow this write? The task is paused, not failed.</div><div class="d">Writes touch your working tree. Reads never ask.</div><div style="margin-top:8px">{k('y','allow once')}{k('a','allow for session')}{k('n','deny, tell the model')}{k('e','edit first')}</div></div>"""
sb1=lb("TASK")+r('<b class="a">◇ Needs you</b>','<span class="d">01:12</span>')+'<div class="d">t_0194a7f4 · turn 4 of 50</div>'+lb("PERMISSIONS")+r("mode","ask for writes")+r("reads","always allowed")+r("shell","ask each time")+r("session grants","0")+lb("PENDING · 1")+'<div><span class="b">write_file</span> <span class="d">retry.rs</span></div><div class="d">idle timer paused while waiting</div>'+lb("IF DENIED")+'<div class="d">The model sees “denied by user” as a tool error and may try another way.</div>'+lb("EVENTS")+'<div class="d">#640 ApprovalRequested<br>#639 ToolQueued ×2</div>'
b1=f'<div class="t">{hd(chr(60)+"span class=a>◇ needs you</span>")}<div class="m"><div class="th">{th1}</div><div class="sb">{sb1}</div></div><div class="cp d">❯ Task paused. Answer the prompt above, or type to queue a follow-up.</div>{ft(k("y","allow")+k("n","deny")+k("a","session")+k("^O","expand")+k("?","help"),"events 640 · dropped 0")}</div>'

# 2 sessions
rows=[("on","0194a7e2",'<span class="s">✓</span> completed',"Inspect this repository, find where model retry…","5 turns · $0.0034 · 00:21","today 14:02"),
("","0194a7e9",'<span class="a">⊘</span> cancelled',"Read every module that touches the event sink…","4 turns · $0.0031 · 02:07","today 14:11"),
("","0194a7f1",'<span class="c">✗</span> failed',"Explain how the DeepSeek adapter normalizes…","2 turns · $0.0009 · 00:46","today 14:26"),
("","0194a7f4",'<span class="a">◇</span> needs you',"Add a jitter test for RetryPolicy…","4 turns · $0.0042 · 01:12","today 14:40")]
rw="".join(f'<div class="rw {o}"><div class="r"><span>{"●" if o else "○"} <b>{i}</b> &nbsp;{s}</span><span class="d">{t}</span></div><div class="d" style="margin-left:18px">{p} <span style="float:right">{m}</span></div></div>' for o,i,s,p,m,t in rows)
md=f"""<div class="md"><div class="r" style="height:44px;align-items:center;border-bottom:1px solid #2a241c"><span><b class="a">◆ sessions</b> <span class="d">/sessions</span></span><span class="d">~/work/loom · 4 of 4</span></div>
<div style="display:flex;gap:20px"><div style="flex:1.4">{lb("FILTER")}<div class="bx" style="margin:0 0 6px">❯ <span class="d">type to filter by task, id or status…</span></div>{lb("RECENT")}{rw}<div class="d" style="margin-top:8px">Resume opens the transcript. A resumed session keeps its history; the model it started with stays recorded.</div></div>
<div style="flex:1;border-left:1px solid #2a241c;padding-left:18px">{lb("PREVIEW · 0194a7e2")}{r("status",'<span class="s">✓ success</span>')}{r("model","deepseek-flash · high")}{r("tool calls","12 · 1 error")}{r("cached",'<span class="g">68%</span>')}{r("events","2,031 · dropped 0")}{lb("LAST ANSWER")}<div class="d">Model retry lives in retry.rs and is driven by the agent loop…</div>{lb("FILES")}<div class="b">retry.rs</div><div class="b">agent/mod.rs</div></div></div>
<div class="r" style="border-top:1px solid #2a241c;padding:10px 20px;margin-top:14px"><span class="d">{k("↑↓","move")}{k("Enter","resume")}{k("e","export NDJSON")}{k("d","delete")}{k("Esc","close")}</span><span class="d">^N new session</span></div></div>"""
b2=f'<div class="t">{hd("")}<div class="m"><div class="th d">❯ Inspect this repository…<br>● turn 1 ✓ 3 calls</div></div>{ft("","")}<div class="ov"></div>{md}</div>'

# 3 compare
runs=[("on","B1","deepseek-flash","high","<span class='s'>✓</span>","5","12","00:21","$0.0034","68%"),
("","B1","deepseek-flash","low","<span class='s'>✓</span>","4","9","00:15","$0.0021","61%"),
("","B1","deepseek-v4-pro","high","<span class='s'>✓</span>","5","11","00:34","$0.0121","66%"),
("","B1","my-model","—","<span class='c'>✗</span>","2","3","00:46","—","—")]
tb="".join(f'<div class="rw {o}" style="display:grid;grid-template-columns:50px 150px 60px 40px 50px 50px 70px 80px 60px"><span>{a}</span><b class="b">{b}</b><span>{c}</span><span>{d}</span><span>{e}</span><span>{f}</span><span>{g}</span><span>{h}</span><span class="g">{i}</span></div>' for o,a,b,c,d,e,f,g,h,i in runs)
th3=f"""<div class="w">Benchmark B1 · read-only · 4 runs <span class="d">(sample data)</span></div>{lb("RUNS")}<div class="d" style="display:grid;grid-template-columns:50px 150px 60px 40px 50px 50px 70px 80px 60px;padding:0 12px"><span>task</span><span>model</span><span>effort</span><span>ok</span><span>turns</span><span>calls</span><span>wall</span><span>cost</span><span>cache</span></div>{tb}
{lb("COST VS WALL TIME")}<div class="bx"><div class="d">deepseek-flash · high</div><div class="bar"><i style="width:28%"></i></div><div class="d">deepseek-flash · low</div><div class="bar"><i style="width:17%"></i></div><div class="d">deepseek-v4-pro · high</div><div class="bar"><i style="width:100%"></i></div></div>
<div class="d">Every run keeps its NDJSON record. Compare replays nothing and calls no model.</div>"""
sb3=lb("SELECTED")+r("<b>B1 · flash · high</b>",'<span class="s">✓ success</span>')+'<div class="d">t_0194a7e2</div>'+lb("VS BASELINE (ROW 1)")+r("cost",'<span class="d">baseline</span>')+r("wall",'<span class="d">baseline</span>')+r("turns",'<span class="d">baseline</span>')+lb("FILTER")+r("task","B1")+r("status","any")+r("model","all")+lb("EXPORT")+'<div class="d">e  runs as NDJSON<br>c  comparison as CSV</div>'
b3=f'<div class="t"><div class="hd"><span class="a">◆ <b>loom</b> <span class="d">runs · compare</span></span><span class="d">~/work/loom · 4 runs</span></div><div class="m"><div class="th">{th3}</div><div class="sb">{sb3}</div></div><div class="cp d">❯ Filter runs, or press / for commands…</div>{ft(k("↑↓","move")+k("Space","mark")+k("Enter","open run")+k("b","set baseline")+k("e","export"),"runs 4 · marked 0")}</div>'

import json,os
PAL=[("ivory","#E9E3D7","text, user input"),("dim","#9A9285","metadata, settled"),("amber","#E3A857","live, attention, brand"),("sage","#86C19F","success"),("coral","#EE8272","error"),("violet","#A99BD6","reasoning, italic"),("blue","#8DB4E8","tool names, paths"),("teal","#6FC4C4","cache, savings"),("thread","#8C6B33","committed thread (decor)"),("dotted","#5A4A2C","provisional (decor)"),("ground","#12100E","background")]
GL=[("●","turn node"),("✓","succeeded"),("✗","failed"),("⚠","interrupted"),("○","queued"),("⊘","cancelled"),("↻","retrying"),("⠇","working"),("▸ ▾","reasoning fold"),("├─ └─","tool batch"),("❯","prompt"),("◆","brand"),("◇","needs you (new)")]
def pg(t,b,w=1200):
    h=page(t,b).replace('"width":1200','"width":%d'%w)
    return h.replace('.t{width:1200px','.t{width:%dpx'%w) if w!=1200 else h
def bxs(h,m,s="",c="",f=("","")):
    return f'<div class="t"><div class="hd">{h}</div><div class="m"><div class="th">{m}</div>{("<div class=\"sb\">"+s+"</div>") if s else ""}</div>{c}{ft(*f)}</div>'
def md(title,body,foot,top=70,left=110,w=980):
    return f'<div class="ov"></div><div class="md" style="top:{top}px;left:{left}px;width:{w}px"><div class="r" style="height:44px;align-items:center;border-bottom:1px solid #2a241c"><b class="a">◆ {title}</b></div>{body}<div class="d" style="border-top:1px solid #2a241c;padding:10px 20px;margin-top:12px">{foot}</div></div>'
dim='<div class="d">❯ Inspect this repository…<br>● turn 1 ✓ 3 calls</div>'
# 4 welcome
b4=f'<div class="t"><div class="hd"><span class="a">◆ <b>loom</b></span><span class="d">first run</span></div><div style="width:620px;margin:56px auto"><div class="a w" style="font-size:22px">◆ loom</div><div class="d">A coding agent for your terminal. One thread per task.</div>{lb("1 · PROVIDER")}<div class="rw on">● deepseek <span class="d">$0.30 in · $1.20 out per M</span></div><div class="rw">○ openai-compatible <span class="d">custom base URL</span></div>{lb("2 · KEY")}<div class="bx">❯ <span class="s">✓</span> DEEPSEEK_API_KEY found in env <div class="d">Or paste a key. Stored in the OS keychain, never in the transcript.</div></div>{lb("3 · TRUST THIS FOLDER")}<div class="bx h">~/work/loom<div class="d">Reads allowed. Writes and shell ask each time. Change later in /tools.</div></div><div style="margin-top:12px">{k("Enter","continue")}{k("Esc","quit")}</div></div></div>'
# 5 idle
s5=lb("SESSION")+r("new","<span class='d'>no turns yet</span>")+r("model","deepseek-flash")+r("effort / thinking","high · on")+lb("DEFAULT BUDGET")+r("turns","50")+r("tool calls","200")+r("wall","10:00")+lb("TOOLS")+r("read","4 on")+r("write","ask")+r("shell","ask")
th5=f'<div class="d" style="margin-top:60px;text-align:center"><div class="a w" style="font-size:20px">◆ loom</div>Describe a task. It runs as one thread.</div>{lb("RECENT")}<div class="rw">❯ Inspect this repository, find where model retry… <span class="d" style="float:right">✓ 5 turns</span></div><div class="rw">❯ Read every module that touches the event sink… <span class="d" style="float:right">⊘ 4 turns</span></div>{lb("TRY")}<div class="d">Explain this crate · Find where X is defined · Add a test for Y</div>'
b5=bxs(hd("idle"),th5,s5,'<div class="cp">❯ <span class="d">Describe a task…</span></div>',(k("Enter","send")+k("/","commands")+k("?","help")+k("/new","new session"),"events 0 · dropped 0"))
# 6 palette
cm=[("/model","pick model, effort, thinking"),("/effort","set reasoning effort"),("/account","switch provider account"),("/sessions","resume a past session"),("/context","inspect what the model sees"),("/tools","tools, permissions, MCP"),("/compact","summarise old turns"),("/export","run record as NDJSON"),("/help","keys and events")]
lst="".join(f'<div class="rw {"on" if i==0 else ""}"><b class="b">{a}</b> <span class="d">{b}</span></div>' for i,(a,b) in enumerate(cm))
kk=[("Esc","cancel task"),("^O","expand result"),("^R","reasoning"),("Tab","sidebar"),("r","retry now"),("y","copy answer"),("e","export"),("^L","redraw")]
kl="".join(f'<div class="r"><span><span class="k">{a}</span></span><span class="d">{b}</span></div>' for a,b in kk)
b6=f'<div class="t">{hd("")}<div class="m"><div class="th">{dim}</div></div>{ft("","")}'+md("commands",f'<div style="display:flex;gap:24px"><div style="flex:1.3"><div class="bx" style="margin:10px 0">❯ /<span class="d">type to filter</span></div>{lst}</div><div style="flex:1;border-left:1px solid #2a241c;padding-left:20px">{lb("KEYS")}{kl}</div></div>',k("↑↓","move")+k("Enter","run")+k("Esc","close"))+'</div>'
# 7 context
seg=[("system + tools","#6b6358",12,"1.9K"),("user","#8DB4E8",1,"0.1K"),("reasoning","#A99BD6",6,"0.9K"),("assistant","#E9E3D7",3,"0.4K"),("tool results","#86C19F",78,"12.7K")]
bar="".join(f'<span style="display:inline-block;height:12px;width:{w}%;background:{c}"></span>' for n,c,w,t in seg)
lg="".join(r(f'<span style="color:{c}">■</span> {n}',t) for n,c,w,t in seg)
it=[("turn 2","read_file agent/mod.rs","6.2K","<span class='g'>cached</span>"),("turn 1","read_file retry.rs","4.1K","<span class='g'>cached</span>"),("turn 2","read_file event.rs","3.0K","—"),("turn 1","search “RetryPolicy”","1.1K","<span class='g'>cached</span>")]
ir="".join(f'<div class="rw" style="display:grid;grid-template-columns:70px 1fr 60px 70px"><span class="d">{a}</span><span class="b">{b}</span><span>{c}</span><span>{d}</span></div>' for a,b,c,d in it)
th7=f'<div class="w">Next request · context</div>{lb("BREAKDOWN")}<div style="margin:4px 0">{bar}</div>{lg}<div class="bx">total (est.) 16.0K · 1.6% of 1M<br><span class="g">cacheable prefix 5.6K ✓ stable</span></div>{lb("LARGEST ITEMS")}{ir}'
s7=lb("ACTIONS")+'<div>'+k("c","compact old turns")+'</div><div style="margin-top:6px">'+k("x","drop selected result")+'</div><div style="margin-top:6px">'+k("p","pin to prefix")+'</div>'+lb("RULES")+'<div class="d">Dropping a result keeps its call line. The model sees “result omitted”.</div>'
b7=bxs(hd("<span class='d'>/context</span>"),th7,s7,"",(k("↑↓","move")+k("Esc","back"),"events 1,284"))
# 8 tools
tl=[("read_file","read","ask never","22"),("search","read","ask never","9"),("list_dir","read","ask never","4"),("write_file","write","ask","0"),("shell","shell","ask each time","0")]
tr="".join(f'<div class="rw {"on" if i==3 else ""}" style="display:grid;grid-template-columns:30px 140px 70px 1fr 40px"><span>{"✓" if i<3 else "◇"}</span><b class="b">{a}</b><span class="d">{b}</span><span>{c}</span><span class="d">{d}</span></div>' for i,(a,b,c,d) in enumerate(tl))
th8=f'<div class="w">Tools &amp; MCP</div>{lb("BUILT-IN")}{tr}{lb("MCP SERVERS")}<div class="rw">● <b>filesystem-docs</b> <span class="s">connected</span> <span class="d">3 tools</span></div><div class="rw">✗ <b>issue-tracker</b> <span class="c">failed</span> <span class="d">spawn error · r retry</span></div>'
s8=lb("write_file")+r("class","write")+r("permission","ask")+r("timeout","30 s")+lb("ARGS")+'<div class="d">path: string<br>content: string</div>'+lb("KEYS")+'<div>'+k("Space","cycle permission")+'</div>'
b8=bxs(hd("<span class='d'>/tools</span>"),th8,s8,"",(k("↑↓","move")+k("Space","permission")+k("r","retry server")+k("Esc","back"),"3 built-in · 2 MCP"))
# 9 narrow
th9=f'<div class="w">❯ Explain the retry loop.</div><div class="d"><span class="a">●</span> turn 1 <span class="s">✓</span> 3 calls · 1.9 s</div><div class="d"><span class="a">●</span> turn 2 <span class="a">⠇ streaming</span></div><div style="border-left:1px solid #8C6B33;padding-left:10px"><span class="v">▸ reasoning · 212 tok</span><div>├─ <span class="s">✓</span> <span class="b">read_file</span> retry.rs</div><div>└─ <span class="a">⠇</span> <span class="b">search</span> “emit(”</div></div>'
b9=f'<div class="t"><div class="hd"><span class="a">◆ <b>loom</b></span><span><span class="a">⠇</span> turn 2 · $0.0012</span></div><div class="m"><div class="th">{th9}</div></div><div class="cp">❯ <span class="d">Task running…</span></div>{ft(k("Esc","cancel")+k("?","help"),"")}</div>'
# 10 spec
sw="".join(f'<div class="r"><span><span style="color:{h}">■</span> {n}</span><span class="d">{h}</span></div>' for n,h,_ in PAL)
gl="".join(r(f'<b class="a">{g}</b>',f'<span class="d">{d}</span>') for g,d in GL)
th10=f'<div style="display:flex;gap:28px"><div style="flex:1">{lb("PALETTE")}{sw}</div><div style="flex:1">{lb("GLYPHS")}{gl}</div><div style="flex:1.2">{lb("GRID")}<div class="d">Monospace 13/20, 1 col ≈ 7.8 px.<br>Wide 120+: thread + 38-col sidebar.<br>Medium 90–119: Tab opens sidebar.<br>Narrow &lt;90: one status line.</div>{lb("RECIPE")}<div class="d">Header 36 px · thread · optional sidebar · composer (amber border) · key hints.<br>Modals: 980 px, amber border, 82% dim.<br>Amber = live or needs you.<br>State = glyph + word + color.</div>{lb("REGENERATE")}<div class="d">python3 loom_wireframes.py<br>then publish project/*.dc.html</div></div></div>'
b10=bxs('<span class="a">◆ <b>loom</b> <span class="d">design spec</span></span>',th10,"","",("","tokens match docs/screenshots"))
SC=[("/model","pick model, effort, thinking"),("/effort <level>","none · low · high · max"),("/account","switch provider account"),("/sessions","resume a past session"),("/new","start a new session"),("/context","inspect what the model sees"),("/tools","tools, permissions, MCP"),("/copy","copy the final answer"),("/export","run record as NDJSON"),("/reasoning","show or hide reasoning"),("/sidebar","focus the sidebar"),("/help","commands and live keys"),("/quit","quit")]
def dd(rows,sel=0):
    return '<div class="bx" style="margin:4px 0;padding:4px 8px">'+"".join(f'<div class="rw {"on" if i==sel else ""}" style="margin:1px 0;padding:2px 8px"><b class="b" style="display:inline-block;width:170px">{a}</b><span class="d">{b}</span></div>' for i,(a,b) in enumerate(rows))+'</div>'
def cmp(t): return f'<div class="cp" style="margin:0">❯ {t}<span class="a">▌</span></div>'
KM=[("y","/copy"),("e","/export"),("^N","/new"),("q","/quit"),("^L","/redraw"),("Tab","/sidebar"),("^R","/reasoning"),("?","/help")]
LV=[("Esc","cancel task"),("^C","cancel · twice quits"),("r","retry now (backoff)"),("↑↓","move between turns"),("^O","expand result"),("↑","edit last task")]
km="".join(f'<div class="r"><span><span class="k">{a}</span></span><span class="b">{b}</span></div>' for a,b in KM)
lv="".join(f'<div class="r"><span><span class="k">{a}</span></span><span class="d">{b}</span></div>' for a,b in LV)
th6=f'<div class="d">{lb("STATE 1 · TYPE /")}</div>{dd(SC[:9])}{cmp("/")}<div class="d">{lb("STATE 2 · TYPE /mo · TAB COMPLETES")}</div>{dd(SC[:1])}{cmp("/mo")}'
sb6=lb("WAS A KEY, NOW A COMMAND")+km+lb("STAYS A KEY · TASK RUNNING")+lv+lb("WHY")+'<div class="d">When idle the composer owns the keyboard, so plain letters type text. Actions move to commands. Live control keeps keys.</div>'
b6=f'<div class="t">{hd("<span class=\'d\'>idle</span>")}<div class="m"><div class="th">{th6}</div><div class="sb" style="width:340px">{sb6}</div></div>{ft(k("/","commands")+k("Tab","complete")+k("↑↓","pick")+k("Enter","run")+k("Esc","dismiss"),"events 2,031 · dropped 0")}</div>'
B=[("Approval","1 · Approval prompt (proposed)",b1,1200),("Sessions","2 · Sessions picker",b2,1200),("Compare","3 · Runs compare",b3,1200),("Welcome","4 · First run: provider, key, trust",b4,1200),("Idle","5 · Idle new session",b5,1200),("Palette","6 · Slash commands",b6,1200),("Context","7 · Context inspector",b7,1200),("Tools","8 · Tools and MCP",b8,1200),("Narrow","9 · Narrow layout (under 90 cols)",b9,640),("Spec","10 · Design spec",b10,1200)]
old=json.load(open("canvas.json")) if os.path.exists("canvas.json") else {}
bd=old.get("boards",{})
for i,(n,t,b,w) in enumerate(B):
    open(n+".dc.html","w").write(pg(t,b,w)); GAL.append((t,b,w))
    f=n+".dc.html"
    if f not in bd: bd[f]={"x":(i%3)*1280,"y":(i//3)*880,"w":w,"h":760,"title":t}
def stp(n):
    L=["providers","type","credentials","verify","model"]
    return '<div style="padding:10px 20px" class="d">'+" &nbsp;·&nbsp; ".join(f'<span class="{"a w" if i+1==n else ""}">{i+1} {x}</span>' for i,x in enumerate(L))+'</div>'
def jb(title,n,body,foot,w=980):
    return f'<div class="t">{hd("")}<div class="m"><div class="th">{dim}</div></div>{ft("","")}'+md(title,stp(n)+f'<div>{body}</div>',foot,top=60,w=w)+'</div>'
def row(on,a,b,c=""): return f'<div class="rw {"on" if on else ""}"><div class="r"><span>{"●" if on else "○"} <b>{a}</b></span><span class="d">{c}</span></div><div class="d" style="margin-left:18px">{b}</div></div>'
def two(l,rr): return f'<div style="display:flex;gap:24px"><div style="flex:1.3">{l}</div><div style="flex:1;border-left:1px solid #2a241c;padding-left:20px">{rr}</div></div>'
nav=k("↑↓","move")+k("Enter","next")+k("Esc","back")
# J1 providers
j1=jb("providers /account",1,two(lb("ACCOUNTS")+row(1,"deepseek · work","key from env · default deepseek-flash","<span class='s'>✓ active</span>")+row(0,"openai-compatible · local","https://llm.internal/v1 · model my-model","<span class='s'>✓ reachable</span>")+'<div class="rw d" style="border:1px dashed #5A4A2C">+ add a provider</div>',lb("DETAILS · work")+r("provider","deepseek")+r("key","env DEEPSEEK_API_KEY")+r("models","2 discovered")+r("last used","today 14:40")+lb("NOTE")+'<div class="d">A task keeps the account it started with. Switching applies from the next task.</div>'),k("↑↓","move")+k("Enter","switch")+k("a","add")+k("t","test")+k("d","remove")+k("Esc","close"))
# J2 type
j2=jb("add provider",2,two(lb("CHOOSE A TYPE")+row(1,"deepseek","hosted API · models discovered for you")+row(0,"openai-compatible","any /v1 endpoint · your own base URL and model id"),lb("YOU WILL NEED")+r("deepseek","an API key")+r("openai-compatible","base URL · key · model id")+lb("AFTER THIS")+'<div class="d">Loom tests the connection before saving anything.</div>'),nav)
# J3 credentials
fld=lambda l,v,h="":f'<div style="margin:6px 0"><span class="d">{l}</span><div class="bx" style="margin:2px 0">❯ {v}</div><div class="d">{h}</div></div>'
j3=jb("credentials",3,two(fld("label","work","Shown in /account and the header.")+fld("base URL <span class='d'>(openai-compatible only)</span>","https://llm.internal/v1")+lb("KEY SOURCE")+row(1,"environment variable","DEEPSEEK_API_KEY · found <span class='s'>✓</span>")+row(0,"paste a key","stored in the OS keychain")+row(0,"command","e.g. pass show llm/key"),lb("HANDLING")+'<div class="d">Keys never enter the transcript, run record or event stream.<br><br>The field shows •••• 3f9a after paste.<br><br>Esc keeps what you typed until you leave this flow.</div>'),k("Tab","next field")+k("Enter","test connection")+k("Esc","back"))
# J4 verify
st=lambda g,t,m:f'<div class="r"><span>{g} {t}</span><span class="d">{m}</span></div>'
ok=lb("TESTING · work")+st("<span class='s'>✓</span>","reach endpoint","120 ms")+st("<span class='s'>✓</span>","auth accepted","")+st("<span class='a'>⠇</span>","list models","")+st("○","probe tools and reasoning","")+'<div class="bar"><i style="width:55%"></i></div>'
bad=lb("IF IT FAILS")+'<div class="bx"><span class="c">✗ auth</span> HTTP 401<div class="d">The key was rejected. Check it, or pick another source.</div>'+k("e","edit key")+k("r","retry")+'</div><div class="bx"><span class="c">✗ transport</span> no route<div class="d">Check the base URL and your network.</div></div><div class="bx"><span class="c">✗ no /models</span><div class="d">Enter the model id by hand next.</div></div>'
j4=jb("verify",4,two(ok+'<div class="d" style="margin-top:8px">Nothing is saved until the test passes or you choose to save anyway.</div>',bad),k("r","retry")+k("s","save anyway")+k("Esc","back"))
# J5 model
j5=jb("choose a model",5,two(lb("DISCOVERED")+row(1,"deepseek-flash","1M context · tools · reasoning","$0.30 / $1.20 per M")+row(0,"deepseek-v4-pro","1M context · tools · reasoning","$1.32 / $3.96 per M")+row(0,"my-model","capabilities unknown","declare below")+lb("DECLARE CAPABILITIES · my-model")+'<div>'+k("t","tools")+k("r","reasoning")+k("c","context size")+'</div>',lb("DEFAULTS")+r("effort","high")+r("thinking","on")+lb("SAVED AS")+'<div class="d">account work · provider deepseek</div>'+lb("SWITCH LATER")+'<div class="bx b">loom auth switch deepseek work</div><div class="s" style="margin-top:8px">✓ ready. Enter starts a task.</div>'),k("↑↓","model")+k("Enter","save and start")+k("Esc","back"))
JB=[("J1","11 · Providers, /account",j1),("J2","12 · Add: choose type",j2),("J3","13 · Add: credentials",j3),("J4","14 · Add: verify connection",j4),("J5","15 · Add: model and defaults",j5)]
for i,(n,t,b) in enumerate(JB):
    f=n+".dc.html"; open(f,"w").write(pg(t,b,1200)); GAL.append((t,b,1200))
    if f not in bd: bd[f]={"x":i*1280,"y":3880,"w":1200,"h":760,"title":t}
nt=old.get("notes",{}); nt["n2"]={"x":0,"y":3600,"text":"Provider setup journey","kind":"title1","maxW":6320}
CB=[]
HT='background:repeating-linear-gradient(135deg,#1a1612 0 6px,#14110e 6px 12px);border:1px dashed #5A4A2C'
sg=[("system + tools","#6b6358",45,"1.9K"),("user","#8DB4E8",2,"0.1K"),("summary of turns 1–12","#A99BD6",74,"3.1K"),("assistant","#E9E3D7",9,"0.4K")]
sbar="".join(f'<span style="display:inline-block;height:12px;width:{w}%;background:{c}"></span>' for n,c,w,t in sg[:1]+sg[2:3]+sg[3:])
sbar='<span style="display:inline-block;height:12px;width:45%;background:#6b6358"></span><span style="display:inline-block;height:12px;width:8%;background:#A99BD6"></span><span style="display:inline-block;height:12px;width:2%;background:#E9E3D7"></span>'
rowsA="".join(f'<div class="rw" style="display:grid;grid-template-columns:70px 1fr 60px 70px"><span class="d">{a}</span><span class="b">{b}</span><span>{c}</span><span>{d}</span></div>' for a,b,c,d in [("turns 1–12","summary (new)","3.1K","—"),("turn 13","read_file retry.rs","1.1K","<span class='g'>cached</span>")])
mA=f'<div class="ov"></div><div class="md" style="top:0;left:40px;width:1120px;height:760px;box-sizing:border-box"><div class="r" style="height:44px;align-items:center;border-bottom:1px solid #2a241c"><b class="a">◆ context <span class="d">/context</span></b><span class="d">after c · compacted</span></div><div>{lb("NEXT REQUEST")}<div>{sbar}</div><div class="bx">total (est.) <b>4.2K</b> · 0.4% of 1M <span class="d">was 16.0K</span></div>{lb("LARGEST ITEMS")}{rowsA}<div style="margin-top:14px">{k("↑↓","move")}{k("c","compact again")}{k("Esc","back")}</div></div><div style="position:absolute;left:20px;right:20px;bottom:12px;height:56px;{HT};padding:8px 14px" class="d">▒ status bar sits under this modal<br>the compact notice is posted there and waits</div></div>'
bA=f'<div class="t">{hd("")}<div class="m"><div class="th">{dim}</div></div>{mA}</div>'
thB=f'<div class="w">❯ Inspect this repository, find where model retry is implemented.</div><div class="d"><span class="a">●</span> turns 1–12 <span class="v">summarised</span></div><div class="d" style="margin:10px 0;border-top:1px dashed #5A4A2C;padding-top:4px">── <span class="a">compacted</span> · 12 turns → 1 summary · 16.0K → 4.2K <span class="d">(proposed thread marker, permanent)</span> ──</div><div class="d"><span class="a">●</span> turn 13 <span class="s">✓</span> 1 call</div>'
sB=lb("TASK")+r('<b class="s">✓ Idle</b>','')+lb("CONTEXT")+r("next request","4.2K")+lb("NOTICE RULES")+'<div class="d">One slot, latest wins.<br>The timer starts when the status bar is uncovered, not when the notice is posted.<br>Stays until the next keypress or 8 s.<br>The modal never reads it. No notice channel.</div>'
ftB=f'<div class="ft" style="border-top:1px solid #E3A857"><span><span class="s">✓</span> <b>compacted</b> 12 turns into 1 summary · 16.0K → 4.2K</span><span>events 2,044 · dropped 0</span></div>'
bB=f'<div class="t">{hd("<span class=\'s\'>✓ idle</span>")}<div class="m"><div class="th">{thB}</div><div class="sb">{sB}</div></div><div class="cp">❯ <span class="d">Next task in this session…</span></div>{ftB}</div>'
for n,t,b,i in (("CompactA","16 · /compact from /context: notice hidden under modal",bA,0),("CompactB","17 · After Esc: notice revealed in status bar",bB,1)):
    f=n+".dc.html"; open(f,"w").write(pg(t,b,1200)); GAL.append((t,b,1200))
    if f not in bd: bd[f]={"x":i*1280,"y":5160,"w":1200,"h":760,"title":t}
    CB.append(f)
nt["n3"]={"x":0,"y":4880,"text":"/compact notice case: modal covers the status bar","kind":"title1","maxW":2480}
cj={"v":3,"createdOnFiles":{"v":1,"at":"2026-11-14T12:00:00Z"},"title":"Loom TUI wireframes","launch":{"view":"canvas"},"pages":[],"boards":bd,"order":[b[0]+".dc.html" for b in B]+[x[0]+".dc.html" for x in JB]+CB,"notes":nt,"designSystems":[]}
json.dump(cj,open("canvas.json","w"))
open("DESIGN.md","w").write("# Loom TUI design language\n\nOne thread per task. Amber is live or needs you; settled is quiet.\n\n## Palette\n"+"\n".join(f"- {n} {h} {u}" for n,h,u in PAL)+"\n\n## Glyphs (LOOM_ASCII=1 fallback)\n"+"\n".join(f"- {g} {d}" for g,d in GL)+"\n\n## Layout\nMonospace 13/20. Wide 120+: thread + 38-col sidebar. Medium 90-119: Tab opens sidebar. Narrow <90: one status line.\nChrome: header, thread, sidebar, composer (amber border), key hints. Modals amber-bordered over dim.\nMeaning is never color alone: glyph + word + color.\n\n## Regenerate\n`python3 loom_wireframes.py` writes project/*.dc.html and canvas.json, then publish.\n")

gal="".join(f'<section><div class="gl">{t}</div>{b.replace(chr(60)+"div class=\"t\">",chr(60)+"div class=\"t\" style=\"width:%dpx\">"%w,1)}</section>' for t,b,w in GAL)
open("index.html","w").write('<!doctype html><html lang="en"><head><meta charset="utf-8"><title>Loom TUI wireframes</title><meta name="viewport" content="width=device-width,initial-scale=1"><link rel="stylesheet" href="https://fonts.googleapis.com/css2?family=JetBrains+Mono:wght@400;700&display=swap"><style>'+CSS+'body{background:#0a0908;color:#9A9285;font:13px monospace;padding:24px}section{display:inline-block;vertical-align:top;margin:0 40px 48px 0}.gl{margin:0 0 8px;color:#E3A857}.zm{position:sticky;top:0;z-index:9;background:#0a0908;padding:6px 0}</style></head><body><div class="zm">zoom <input type="range" min="40" max="100" value="60" oninput="document.getElementById(\'g\').style.zoom=this.value/100"> &nbsp; '+str(len(GAL))+' screens</div><div id="g" style="zoom:.6">'+gal+'</div></body></html>')
print("wrote",len(GAL),"screens to",os.getcwd())
open("DESIGN.md","a").write("\n## Commands\nSlash-first. Idle: the composer owns the keyboard, so actions are commands (/model /effort /account /sessions /new /context /tools /copy /export /reasoning /sidebar /help /quit). Inline list above the composer, filters as you type, Tab completes.\nLive keys kept while a task runs: Esc, ^C, r, up/down, ^O, up (edit last task).\n\n## Compact notice\nPosted to the status bar; a modal (/context) may cover it. One slot, latest wins; timer starts when the status bar is uncovered; stays until next keypress or 8 s.\n")
