// Text selection inside a rendered office document.
//
// The host's selection engine (src/selection/) cannot serve these previews: the
// frame's origin is opaque, so `caretFromPoint` and the `[data-selectable]` walk
// have nothing to reach. This is the same model reimplemented on the far side of
// the frame boundary - carets, drag granularity, keyboard caret mode, accent
// highlight rects - while the *host* keeps owning intent: the copy and
// search-selection chords, the footer hints and the popover all run there, over
// the text this script posts out. See `adoptExternal` in selection/controller.ts.
//
// Three things had to be built here rather than reused:
//
//  - **Carets, because nothing native may start.** The frame preventDefaults every
//    mousedown to keep focus pinned on the host's search input (see the
//    focus-custody note in `officeBootstrap`), and that also cancels the browser's
//    own selection gesture. Positions come from `caretRangeFromPoint` instead.
//  - **Rects, because WebKit will not paint a selection in an unfocused frame** in
//    the host's accent - the same reason the host engine draws its own. Ours are
//    divs in an overlay *inside* the transformed content, so they need no
//    recompute when the reader pans or zooms.
//  - **TSV, because `Range.toString()` welds cells together.** A grid selection
//    has to come out as tab/newline separated text or it pastes back as garbage.
//
// What is *not* reimplemented: word and line boundaries, and vertical caret
// movement. Those come from `Selection.modify`, which is ~250 lines of WebKit
// behaviour (bidi, wrapped lines, the sticky goal column for Up/Down runs) that
// would be silly to approximate. The native selection is therefore kept live as
// the caret's home so that goal column survives a run of arrow keys - and its
// painting is suppressed with `::selection{background:transparent}`, since ours
// is drawn on top.

export interface FrameSelectionOpts {
  /**
   * Elements whose text may be selected. Doubles as the select-vs-pan
   * discriminator: a press inside one starts a selection, a press anywhere else
   * starts a pan - the rule the PDF text layer follows.
   */
  text: string;
  /**
   * Chrome a drag may cross but must never select or highlight (a sheet's row and
   * column gutter). Empty string disables the check.
   */
  exclude: string;
  /**
   * Box the highlight rects are written into, when the variant has one that is
   * *the content*: a slide's canvas is centred in a letterbox whose size follows
   * the viewport, so rects pinned to the wrapper drift off the text the moment
   * the panel is resized. Empty string falls back to inferring it from whatever
   * the document scrolls in.
   */
  host: string;
}

/** Elements that end a line when text is pulled out of a prose document. */
const BLOCK_TAGS = 'P,DIV,LI,TR,TD,TH,H1,H2,H3,H4,H5,H6,BLOCKQUOTE,PRE,SECTION';

/**
 * The engine as a function expression, for `officeBootstrap` to call with the
 * frame utilities it already owns:
 *
 *     var SEL = <script>(post, vpBox, pan, getZoom, wrapper, setCursor);
 *
 * `vpBox()` is the frame's viewport in *client* px - the space `getClientRects`
 * answers in, which is what every measurement here is compared against - and
 * `pan(dx,dy)` moves the document by a client-px delta. Neither is a scroller:
 * the reader positions its content with one transform and nothing in this
 * document scrolls (see the scaffold note in srcdoc.ts).
 *
 * Returns `{ msg, moved }` - `msg` handles the host's `sel*` messages, `moved` is
 * the cue that the reader moved the document under a live selection, so the
 * popover anchor (which the host holds in frame-viewport coordinates) is restated.
 *
 * The body below is a template literal, so it must contain no backtick and no
 * `${`. Comments inside it therefore quote identifiers as `name()`, not in
 * backticks like the rest of this codebase does.
 */
export function officeSelectionScript(o: FrameSelectionOpts): string {
  const TEXT = JSON.stringify(o.text);
  const EXCL = JSON.stringify(o.exclude);
  const HOST = JSON.stringify(o.host);
  const BLOCK = JSON.stringify(BLOCK_TAGS);
  return `(function(post,vpBox,pan,getZ,W,setCursor){
var TEXT=${TEXT},EXCL=${EXCL},HOST=${HOST},BLOCK=${BLOCK};
var PAD=14;

/* ── state ─────────────────────────────────────────────────────────────────── */
/* sa/sf are the anchor and focus as {n:node,o:offset}; sa null means collapsed.
   The native selection mirrors them (see the header). */
var sa=null,sf=null,kb=false;
var ov=null,ovHost=null,ovProbe=null,ovRects=null;
var PROBE=100;
var gran='char',ab=null;
var pend=null,dragging=false,panning=null,moveTimer=0;

/* ── dom helpers ───────────────────────────────────────────────────────────── */
var isEl=function(n){return n&&n.nodeType===1;};
var el=function(n){return isEl(n)?n:(n?n.parentElement:null);};
var inSel=function(n){var e=el(n);if(!e)return false;
  if(EXCL&&e.closest(EXCL))return false;
  return !!e.closest(TEXT);};
var firstText=function(n){if(!n)return null;if(n.nodeType===3)return n;
  var w=document.createTreeWalker(n,4,null,false);return w.nextNode();};
var lastText=function(n){if(!n)return null;if(n.nodeType===3)return n;
  var w=document.createTreeWalker(n,4,null,false),t=null,x;
  while((x=w.nextNode()))t=x;return t;};

/* An element-boundary hit resolved to the nearest text position in document
   order - caretRangeFromPoint hands back an element container often enough that
   every consumer below would otherwise need to special-case it. */
var toText=function(p){
  if(!p)return null;
  if(p.n.nodeType===3)return p;
  var c=p.n.childNodes,i=Math.min(p.o,Math.max(c.length-1,0)),k,t;
  for(k=i;k<c.length;k++){t=firstText(c[k]);if(t)return{n:t,o:0};}
  for(k=i;k>=0;k--){t=lastText(c[k]);if(t)return{n:t,o:t.data.length};}
  t=firstText(p.n);return t?{n:t,o:0}:null;
};

var caretAt=function(x,y){
  var p=null;
  if(document.caretRangeFromPoint){
    var r=document.caretRangeFromPoint(x,y);
    if(r)p={n:r.startContainer,o:r.startOffset};
  }else if(document.caretPositionFromPoint){
    var q=document.caretPositionFromPoint(x,y);
    if(q)p={n:q.offsetNode,o:q.offset};
  }
  p=toText(p);
  return p&&inSel(p.n)?p:null;
};

/* ── ranges ────────────────────────────────────────────────────────────────── */
/* Forward range between two positions. Setting an end before the start collapses
   the range per spec, which is how a backwards drag is detected and flipped. */
var mk=function(a,b){
  var r=document.createRange();
  try{r.setStart(a.n,a.o);r.setEnd(b.n,b.o);}catch(e){return null;}
  if(!r.collapsed)return r;
  var v=document.createRange();
  try{v.setStart(b.n,b.o);v.setEnd(a.n,a.o);}catch(e2){return null;}
  return v.collapsed?null:v;
};
var cur=function(){return(sa&&sf&&sa.n.isConnected&&sf.n.isConnected)?mk(sa,sf):null;};

/* Text nodes a range touches, minus excluded chrome. Walks nodes rather than
   using Range.getClientRects, which also reports the boxes of whole contained
   elements (a table row's own rect, in a grid). */
var nodesIn=function(r){
  var ca=r.commonAncestorContainer;
  if(ca.nodeType===3)return inSel(ca)?[ca]:[];
  var out=[],w=document.createTreeWalker(ca,4,null,false),n;
  while((n=w.nextNode())){
    if(!n.data.length)continue;
    if(!r.intersectsNode(n))continue;
    if(!inSel(n))continue;
    out.push(n);
  }
  return out;
};
var offs=function(r,n){
  return[n===r.startContainer?r.startOffset:0,
         n===r.endContainer?r.endOffset:n.data.length];
};

/* ── native selection as the navigation oracle ─────────────────────────────── */
var nsel=function(){return document.getSelection();};
/* Push our state into the native selection. Called after every *mouse* action;
   deliberately NOT called between keyboard steps, because assigning the selection
   resets WebKit's goal column and a run of Up/Down would then drift sideways. */
var sync=function(){
  var s=nsel();if(!s)return;
  try{
    if(sa&&sf)s.setBaseAndExtent(sa.n,sa.o,sf.n,sf.o);
    else if(sf)s.collapse(sf.n,sf.o);
    else s.removeAllRanges();
  }catch(e){}
};
/* One boundary probe: move a collapsed selection by one unit and read where it
   landed. Used only for click granularity, where there is no goal column to keep. */
/* modify() can land on an element boundary; normalising here keeps every consumer
   below free of that case - offs() in particular only recognises a text container
   as the range's own endpoint, and would otherwise over-select the first node. */
var probe=function(p,dir,unit){
  var s=nsel();if(!s)return null;
  try{s.collapse(p.n,p.o);s.modify('move',dir,unit);}catch(e){return null;}
  return s.focusNode?toText({n:s.focusNode,o:s.focusOffset}):null;
};
/* [start,end] of the word or line around a position. */
var bounds=function(p,g){
  if(g==='char')return[p,p];
  var unit=g==='word'?'word':'lineboundary';
  var a=probe(p,'backward',unit);if(!a)return null;
  var b=probe(a,'forward',unit);if(!b)return null;
  return[a,b];
};
/* True when a is at or before b in document order. */
var le=function(a,b){
  var r=mk(a,b);
  if(!r)return true;
  return r.startContainer===a.n&&r.startOffset===a.o;
};

/* ── overlay ───────────────────────────────────────────────────────────────── */
/* The overlay is parented into the content itself - the variant's own box where it
   has one, the zoom wrapper otherwise - so the rects ride every pan and zoom for
   free. It sits below the frozen panes' z-index on purpose: they cover cells, so
   they must cover rects too. */
var ensure=function(){
  var p=HOST?document.querySelector(HOST):null;
  if(!p)p=W||document.body;
  if(ovHost!==p){
    if(ov&&ov.parentNode)ov.parentNode.removeChild(ov);
    ov=null;ovProbe=null;ovRects=null;ovHost=p;
  }
  if(!ov){
    /* Rects position from this box's origin, so it has to be one. Every emitter
       gives its grid a positioned wrapper, but a static parent would silently
       shift the whole overlay to the nearest ancestor that is - and relative with
       no offsets changes nothing but the containing block. */
    if(getComputedStyle(ovHost).position==='static')ovHost.style.position='relative';
    ov=document.createElement('div');ov.className='osel';
    ov.setAttribute('aria-hidden','true');
    /* A hidden box of known CSS size, in the same coordinate space the rects are
       written in - see scaleOf(). Persistent, so writing the rects does not wipe
       it, which is why they go in their own child rather than straight into ov. */
    ovProbe=document.createElement('i');ovProbe.className='osp';
    ovRects=document.createElement('div');ovRects.className='osx';
    ov.appendChild(ovProbe);ov.appendChild(ovRects);
    ovHost.appendChild(ov);
  }
};
/* Merge per-fragment rects into one bar per visual line (mirrors the host
   engine's mergeLineRects, so a multi-line selection looks the same). */
var merge=function(rs){
  if(rs.length<2)return rs;
  rs.sort(function(a,b){return a[1]-b[1]||a[0]-b[0];});
  var out=[],i,r,l;
  for(i=0;i<rs.length;i++){
    r=rs[i];l=out[out.length-1];
    if(l){
      var ov2=Math.min(l[1]+l[3],r[1]+r[3])-Math.max(l[1],r[1]);
      if(ov2>0.5*Math.min(l[3],r[3])){
        var x=Math.min(l[0],r[0]),y=Math.min(l[1],r[1]);
        l[2]=Math.max(l[0]+l[2],r[0]+r[2])-x;
        l[3]=Math.max(l[1]+l[3],r[1]+r[3])-y;
        l[0]=x;l[1]=y;continue;
      }
    }
    out.push([r[0],r[1],r[2],r[3]]);
  }
  return out;
};
var caretBox=function(p){
  var r=document.createRange();
  try{r.setStart(p.n,p.o);}catch(e){return null;}
  r.collapse(true);
  var rs=r.getClientRects();
  if(rs.length)return rs[0];
  var len=p.n.data?p.n.data.length:0;
  if(!len)return null;
  var at=Math.min(p.o,len-1),q=document.createRange();
  try{q.setStart(p.n,at);q.setEnd(p.n,at+1);}catch(e2){return null;}
  var b=q.getBoundingClientRect();
  if(b.height<=0)return null;
  return{left:p.o>=len?b.right:b.left,top:b.top,width:0,height:b.height};
};
/* Painted pixels per authored CSS pixel, in the space the rects are written in.
   Measured, not assumed, because TWO independent factors scale this subtree: the
   reader's zoom (a transform on the wrapper) and the launcher's UI scale (zoom on
   :root, which the frame re-applies because WebKitGTK hands a zoomed frame a
   layout viewport that disagrees with its painted box). Dividing by only one of
   them puts every rect out by a fixed percentage of its distance from the origin -
   correct at the top of a sheet, a row off by row 50.
   The probe is the honest way to get it: a box at a known CSS size pinned to the
   rects' own origin, so the ratio of its painted rect to that size is the
   conversion by definition - no assumption about how offsetWidth reports zoom -
   and its painted position is that origin, with no assumption about the parent's
   padding either. Returns [originX, originY, scaleX, scaleY]. */
var frame0=function(){
  var z=getZ()||1;
  var h=ovHost.getBoundingClientRect();
  if(!ovProbe)return[h.left,h.top,z,z];
  var r=ovProbe.getBoundingClientRect();
  return[r.left,r.top,r.width>0?r.width/PROBE:z,r.height>0?r.height/PROBE:z];
};
/* Rects are written in the overlay parent's own layout coordinates, so they are
   free on scroll and on zoom - whatever scales the content scales them
   identically, and neither needs a recompute. */
var draw=function(){
  var r=cur();
  if(!r&&!kb){if(ovRects)ovRects.innerHTML='';return;}
  ensure();
  var f=frame0(),OX=f[0],OY=f[1],SX=f[2],SY=f[3];
  var html='',rs=[],i,n,ofs,sub,cr,j;
  if(r){
    sub=document.createRange();
    var ns=nodesIn(r);
    for(i=0;i<ns.length;i++){
      n=ns[i];ofs=offs(r,n);
      if(ofs[0]>=ofs[1])continue;
      try{sub.setStart(n,ofs[0]);sub.setEnd(n,ofs[1]);}catch(e){continue;}
      cr=sub.getClientRects();
      for(j=0;j<cr.length;j++){
        if(cr[j].width<=0||cr[j].height<=0)continue;
        rs.push([(cr[j].left-OX)/SX,(cr[j].top-OY)/SY,cr[j].width/SX,cr[j].height/SY]);
      }
    }
    rs=merge(rs);
    for(i=0;i<rs.length;i++){
      html+='<i class="osr" style="left:'+rs[i][0]+'px;top:'+rs[i][1]+'px;width:'+rs[i][2]+'px;height:'+rs[i][3]+'px"></i>';
    }
  }
  if(kb){
    var cb=sf?caretBox(sf):null;
    if(cb)html+='<i class="osc'+(moveTimer?' mv':'')+'" style="left:'+((cb.left-OX)/SX)+'px;top:'+((cb.top-OY)/SY)+'px;height:'+(cb.height/SY)+'px"></i>';
  }
  ovRects.innerHTML=html;
};
/* The popover lives in the host document, so its anchor crosses the boundary in
   frame-viewport pixels - the one space both sides can agree on. Taken from the
   focus end, as the host engine does: the bottom-most rect is the wrong end of an
   upward selection, and reading one caret box is cheap enough to redo on scroll. */
var anchorOf=function(){
  var cb=sf?caretBox(sf):null;
  return cb?[cb.left,cb.top,cb.width,cb.height]:null;
};

/* ── text extraction ───────────────────────────────────────────────────────── */
var cellOf=function(n){var e=el(n);return e?e.closest('td,th'):null;};
var rowOf=function(n){var e=el(n);return e?e.closest('tr'):null;};
var blockOf=function(n){var e=el(n);return e?e.closest(BLOCK):null;};
/* Tabs from one cell to the next, one per intervening cell - so an empty column
   inside the selection still shifts what follows it into the right field. */
var gapTabs=function(a,b){
  var k=0,e=a.nextSibling,hit=false;
  while(e){
    if(e===b){hit=true;break;}
    if(e.nodeType===1&&(e.tagName==='TD'||e.tagName==='TH'))k++;
    e=e.nextSibling;
  }
  if(!hit)k=0;
  var s='';for(var i=0;i<=k;i++)s+='\\t';
  return s;
};
/* Tab between cells, newline between rows: a grid selection has to paste back
   into a spreadsheet as a grid. A prose document has neither, and falls back to a
   newline per block. */
var textOf=function(r){
  if(!r)return '';
  var ns=nodesIn(r),out='',pc=null,pr=null,pb=null,i,n,ofs,c,rw,b;
  for(i=0;i<ns.length;i++){
    n=ns[i];ofs=offs(r,n);
    c=cellOf(n);rw=rowOf(n);b=c||blockOf(n);
    if(i){
      if(pr&&rw&&pr!==rw)out+='\\n';
      else if(pc&&c&&pc!==c)out+=gapTabs(pc,c);
      else if(!c&&pb&&b&&b!==pb)out+='\\n';
    }
    out+=n.data.slice(ofs[0],ofs[1]);
    pc=c;pr=rw;pb=b;
  }
  return out;
};

/* ── publishing ────────────────────────────────────────────────────────────── */
var last='';
var send=function(){
  var vb=vpBox();
  post({type:'sel',text:last,keyboard:kb,dragging:dragging,
        anchor:anchorOf(),vw:vb.w,vh:vb.h});
};
var emit=function(){
  var r=cur();
  last=r?textOf(r):'';
  draw();send();
};
var clear=function(){
  if(!sa&&!sf&&!kb)return;
  sa=null;sf=null;kb=false;gran='char';ab=null;
  sync();emit();
};

/* ── caret reveal ──────────────────────────────────────────────────────────── */
/* The caret is kept a PAD inside the viewport by panning the document, not by
   scrolling it - there is no scroller. A caret above the top means the content has
   to come down, which is a positive dy, and the deltas are in client px because
   that is what a caret box is measured in. */
var reveal=function(){
  var cb=sf?caretBox(sf):null;
  if(!cb)return;
  var vb=vpBox(),dx=0,dy=0;
  if(cb.top<PAD)dy=PAD-cb.top;
  else if(cb.top+cb.height>vb.h-PAD)dy=-(cb.top+cb.height-(vb.h-PAD));
  if(cb.left<PAD)dx=PAD-cb.left;
  else if(cb.left>vb.w-PAD)dx=-(cb.left-(vb.w-PAD));
  if(dx||dy)pan(dx,dy);
};

/* ── keyboard caret mode ───────────────────────────────────────────────────── */
var MOVE={ArrowLeft:['left','character'],ArrowRight:['right','character'],
          ArrowUp:['backward','line'],ArrowDown:['forward','line'],
          Home:['backward','lineboundary'],End:['forward','lineboundary']};
var firstVisible=function(){
  var vb=vpBox(),els=document.querySelectorAll(TEXT),vh=vb.h,vw=vb.w,i,b,t;
  for(i=0;i<els.length;i++){
    b=els[i].getBoundingClientRect();
    if(b.bottom>0&&b.top<vh&&b.right>0&&b.left<vw){
      t=firstText(els[i]);
      if(t&&t.data.length)return{n:t,o:0};
    }
  }
  return null;
};
var enter=function(){
  var p=(sf&&sf.n.isConnected)?sf:firstVisible();
  if(!p){clear();return;}
  sf=p;sa=null;kb=true;gran='char';ab=null;
  sync();reveal();emit();
};
/* Movement runs on the *native* selection with no re-seat in between, which is
   what keeps WebKit's goal column alive across a run of Up/Down. Shift maps to
   its own 'extend' so the anchor bookkeeping is WebKit's too. */
var step=function(k,shift,ctrl){
  var m=MOVE[k];if(!m)return false;
  var s=nsel();if(!s)return true;
  if(!sf||!sf.n.isConnected){enter();if(!sf)return true;}
  var unit=(ctrl&&m[1]==='character')?'word':m[1];
  try{s.modify(shift?'extend':'move',m[0],unit);}catch(e){return true;}
  if(!s.focusNode)return true;
  var nf=toText({n:s.focusNode,o:s.focusOffset});
  if(!nf)return true;
  sf=nf;
  sa=shift&&s.anchorNode?toText({n:s.anchorNode,o:s.anchorOffset}):null;
  /* Hold the caret solid while it is moving; the blink makes a travelling cursor
     hard to follow (same 500ms as the host engine's). */
  if(moveTimer)clearTimeout(moveTimer);
  moveTimer=setTimeout(function(){moveTimer=0;emit();},500);
  reveal();emit();
  return true;
};

/* ── mouse ─────────────────────────────────────────────────────────────────── */
/* Left press on text starts a selection; left press anywhere else, or a middle
   press anywhere, starts a pan. Identical to the PDF reader's rule, where drags
   that begin on the text layer select and everything else moves the page.
   The default is cancelled either way - see the focus-custody note. */
document.addEventListener('mousedown',function(e){
  e.preventDefault();
  var onText=e.button===0&&inSel(e.target);
  if(!onText){
    if(e.button!==0&&e.button!==1)return;
    clear();
    panning={x:e.clientX,y:e.clientY};
    setCursor('grabbing');
    return;
  }
  if(e.detail>=2){
    var p=caretAt(e.clientX,e.clientY),g=e.detail===2?'word':'line',bb=p?bounds(p,g):null;
    if(!bb)return;
    sa=bb[0];sf=bb[1];kb=false;gran=g;ab=bb;
    pend={x:e.clientX,y:e.clientY};
    sync();emit();
    return;
  }
  pend={x:e.clientX,y:e.clientY};gran='char';ab=null;
},true);

document.addEventListener('mousemove',function(e){
  if(panning){
    pan(e.clientX-panning.x,e.clientY-panning.y);
    panning.x=e.clientX;panning.y=e.clientY;
    return;
  }
  if(!pend)return;
  if(!dragging){
    var dx=e.clientX-pend.x,dy=e.clientY-pend.y;
    if(dx*dx+dy*dy<9)return;
    if(gran==='char'){
      var a=caretAt(pend.x,pend.y);
      if(!a)return;
      sa=a;sf=a;
    }
    kb=false;dragging=true;
  }
  var f=caretAt(e.clientX,e.clientY);
  if(f){
    if(gran==='char'||!ab)sf=f;
    else{
      /* Granular drag: union the click's word/line with the one under the pointer,
         keeping the far edge pinned. */
      var fb=bounds(f,gran)||[f,f];
      if(le(fb[1],ab[0])){sa=ab[1];sf=fb[0];}
      else{sa=ab[0];sf=fb[1];}
    }
    sync();
  }
  emit();
});

var endDrag=function(){
  var wasDrag=dragging,wasGran=gran!=='char';
  pend=null;dragging=false;gran='char';ab=null;
  if(panning){panning=null;setCursor('');}
  if(!wasDrag){
    /* A double/triple click that never moved keeps its word/line selection; a
       plain click collapses, like a native one. */
    if(!wasGran)clear();
    else emit();
    return;
  }
  if(!cur())clear();
  else emit();
};
document.addEventListener('mouseup',endDrag);
document.addEventListener('mouseleave',endDrag);
window.addEventListener('blur',endDrag);

/* A pan or a zoom moves the popover's anchor without touching the selection: the
   rects need no recompute (they live inside the transformed content), so only the
   anchor is restated - coalesced to one message per frame, because a trackpad
   emits view changes far faster than the host can render. Called as moved() from
   the reader's apply(), which is the only thing that can move the document. */
var pending=0;
var restate=function(){
  if(!(sa||sf||kb)||pending)return;
  pending=requestAnimationFrame(function(){pending=0;send();});
};

return{
  msg:function(d){
    switch(d.type){
      case 'selEnter':enter();return true;
      case 'selClear':clear();return true;
      case 'selKey':
        if(!kb)return true;
        if(d.key==='Escape'){clear();return true;}
        step(d.key,!!d.shift,!!d.ctrl);
        return true;
    }
    return false;
  },
  moved:restate
};
})`;
}
