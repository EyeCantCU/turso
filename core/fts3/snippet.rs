//! snippet(), offsets() and matchinfo() (`fts3_snippet.c`).

use super::table::{Ctx, CursorState};
use super::{corrupt, get_varint, sql_error, value_text, Sql};
use crate::{LimboError, Result, Value};

fn tokens_of(st: &CursorState, text: &[u8]) -> Result<Vec<super::tokenizer::TokenInfo>> {
    let tok = st.tab.tokenizer(Some(&st.conn))?;
    super::tokenizer::tokens(tok.as_ref(), st.langid, text)
}

#[derive(Default, Clone)]
struct SnipPhrase {
    ntoken: usize,
    list: Vec<i64>,
    has_list: bool,
    head: Option<usize>,
    ihead: i64,
    tail: Option<usize>,
    itail: i64,
}

fn snip_advance(list: &[i64], it: &mut Option<usize>, val: &mut i64, next: i64) {
    if let Some(mut k) = *it {
        while *val < next {
            if k + 1 >= list.len() {
                *val = -1;
                *it = None;
                return;
            }
            k += 1;
            *val = list[k];
        }
        *it = Some(k);
    }
}

#[derive(Default, Clone, Copy)]
struct Fragment {
    icol: i32,
    ipos: i32,
    covered: u64,
    hlmask: u64,
}

/// `fts3BestSnippet`.
fn best_snippet(
    st: &mut CursorState,
    nsnippet: i32,
    icol: i32,
    covered: u64,
    seen: &mut u64,
) -> Result<(Fragment, i32)> {
    let ev = st.eval.as_mut().expect("full-text cursor");
    let phrases = ev.expr.phrases();
    let mut sp: Vec<SnipPhrase> = vec![SnipPhrase::default(); phrases.len()];
    for _ in 0..2 {
        for (i, &e) in phrases.iter().enumerate() {
            let p = &mut sp[i];
            p.ntoken = ev.ph[e].as_ref().map(|x| x.ntoken).unwrap_or(0);
            if let Some(list) = ev.phrase_poslist(e, icol) {
                if list[0] < 0 {
                    return Err(corrupt());
                }
                p.ihead = list[0];
                p.itail = list[0];
                p.head = Some(0);
                p.tail = Some(0);
                p.list = list;
                p.has_list = true;
            }
        }
    }
    for (i, p) in sp.iter().enumerate() {
        if p.head.is_some() {
            *seen |= 1u64 << (i % 64);
        }
    }
    let mut frag = Fragment {
        icol,
        ..Default::default()
    };
    let mut best = -1;
    let mut current: i32 = -1;
    loop {
        // fts3SnippetNextCandidate
        if current < 0 {
            current = 0;
            for p in sp.iter_mut() {
                let l = p.list.clone();
                snip_advance(&l, &mut p.head, &mut p.ihead, nsnippet as i64);
            }
        } else {
            let mut iend = 0x7FFFFFFFi64;
            for p in &sp {
                if p.head.is_some() && p.ihead < iend {
                    iend = p.ihead;
                }
            }
            if iend == 0x7FFFFFFF {
                break;
            }
            let istart = iend - nsnippet as i64 + 1;
            current = istart as i32;
            for p in sp.iter_mut() {
                let l = p.list.clone();
                snip_advance(&l, &mut p.head, &mut p.ihead, iend + 1);
                snip_advance(&l, &mut p.tail, &mut p.itail, istart);
            }
        }
        // fts3SnippetDetails
        let istart = current as i64;
        let mut score = 0;
        let mut cover = 0u64;
        let mut hl = 0u64;
        for (i, p) in sp.iter().enumerate() {
            if let Some(mut k) = p.tail {
                let mut icsr = p.itail;
                while icsr < istart + nsnippet as i64 && icsr >= istart {
                    let mphrase = 1u64 << (i % 64);
                    let mpos = 1u64 << (icsr - istart);
                    if (cover | covered) & mphrase != 0 {
                        score += 1;
                    } else {
                        score += 1000;
                    }
                    cover |= mphrase;
                    let mut j = 0;
                    while j < p.ntoken && j < nsnippet as usize {
                        hl |= mpos >> j;
                        j += 1;
                    }
                    if k + 1 >= p.list.len() {
                        break;
                    }
                    k += 1;
                    icsr = p.list[k];
                }
            }
        }
        if score > best {
            frag.ipos = current;
            frag.hlmask = hl;
            frag.covered = cover;
            best = score;
        }
    }
    Ok((frag, best))
}

/// `fts3SnippetShift`.
fn snippet_shift(
    st: &CursorState,
    nsnippet: i32,
    doc: &[u8],
    ipos: &mut i32,
    hlmask: &mut u64,
) -> Result<()> {
    let hl = *hlmask;
    if hl != 0 {
        let mut nleft = 0;
        while hl & (1u64 << nleft) == 0 {
            nleft += 1;
        }
        let mut nright = 0;
        while hl & (1u64 << (nsnippet - 1 - nright)) == 0 {
            nright += 1;
        }
        let ndesired = (nleft - nright) / 2;
        if ndesired > 0 {
            let toks = tokens_of(st, doc)?;
            let mut cur = 0;
            let mut done = false;
            let mut it = toks.iter();
            while cur < nsnippet + ndesired {
                match it.next() {
                    Some(t) => cur = t.3,
                    None => {
                        done = true;
                        break;
                    }
                }
            }
            let nshift = done as i32 + cur - nsnippet;
            if nshift > 0 {
                *ipos += nshift;
                *hlmask = hl >> nshift;
            }
        }
    }
    Ok(())
}

/// `fts3SnippetText`.
#[allow(clippy::too_many_arguments)]
fn snippet_text(
    st: &mut CursorState,
    frag: &Fragment,
    ifrag: usize,
    is_last: bool,
    nsnippet: i32,
    open: &[u8],
    close: &[u8],
    ellipsis: &[u8],
    out: &mut Vec<u8>,
) -> Result<()> {
    let Some(doc) = st.column_text(frag.icol as usize)? else {
        return Ok(());
    };
    let mut ipos = frag.ipos;
    let mut hlmask = frag.hlmask;
    let toks = tokens_of(st, &doc)?;
    let mut iend = 0usize;
    let mut shift_done = false;
    let mut it = toks.into_iter();
    loop {
        let Some((_, ibegin, ifin, icur)) = it.next() else {
            out.extend_from_slice(&doc[iend.min(doc.len())..]);
            break;
        };
        let (ibegin, ifin) = (ibegin as usize, ifin as usize);
        if icur < ipos {
            continue;
        }
        if !shift_done {
            snippet_shift(st, nsnippet, &doc[ibegin..], &mut ipos, &mut hlmask)?;
            shift_done = true;
            if ipos > 0 || ifrag > 0 {
                out.extend_from_slice(ellipsis);
            } else if ibegin > 0 {
                out.extend_from_slice(&doc[..ibegin]);
            }
            if icur < ipos {
                continue;
            }
        }
        if icur >= ipos + nsnippet {
            if is_last {
                out.extend_from_slice(ellipsis);
            }
            break;
        }
        let hl = hlmask & (1u64 << (icur - ipos)) != 0;
        if icur > ipos {
            out.extend_from_slice(&doc[iend..ibegin]);
        }
        if hl {
            out.extend_from_slice(open);
        }
        out.extend_from_slice(&doc[ibegin..ifin]);
        if hl {
            out.extend_from_slice(close);
        }
        iend = ifin;
    }
    Ok(())
}

/// `sqlite3Fts3Snippet`.
pub(crate) fn snippet(
    st: &mut CursorState,
    open: &[u8],
    close: &[u8],
    ellipsis: &[u8],
    icol: i32,
    ntoken: i32,
) -> Result<Vec<u8>> {
    if st.eval.as_ref().is_none_or(|e| e.expr.root.is_none()) {
        return Ok(Vec::new());
    }
    let ncol = st.tab.ncol() as i32;
    let mut frags = [Fragment::default(); 4];
    let mut nsnippet = 1usize;
    let mut nftoken;
    loop {
        let mut covered = 0u64;
        let mut seen = 0u64;
        nftoken = if ntoken >= 0 {
            (ntoken + nsnippet as i32 - 1) / nsnippet as i32
        } else {
            -ntoken
        };
        for frag in frags.iter_mut().take(nsnippet) {
            let mut best = -1;
            *frag = Fragment::default();
            for iread in 0..ncol {
                if icol >= 0 && iread != icol {
                    continue;
                }
                let (f, s) = best_snippet(st, nftoken, iread, covered, &mut seen)?;
                if s > best {
                    *frag = f;
                    best = s;
                }
            }
            covered |= frag.covered;
        }
        if seen == covered || nsnippet == frags.len() {
            break;
        }
        nsnippet += 1;
    }
    let mut out = Vec::new();
    for (i, f) in frags.iter().take(nsnippet).enumerate() {
        snippet_text(
            st,
            f,
            i,
            i == nsnippet - 1,
            nftoken,
            open,
            close,
            ellipsis,
            &mut out,
        )?;
    }
    Ok(out)
}

/// `sqlite3Fts3Offsets`.
pub(crate) fn offsets(st: &mut CursorState) -> Result<String> {
    if st.eval.as_ref().is_none_or(|e| e.expr.root.is_none()) {
        return Ok(String::new());
    }
    st.eval.as_mut().unwrap().cancel_incr();
    let ncol = st.tab.ncol();
    let mut res = String::new();
    for icol in 0..ncol {
        struct Term {
            list: Vec<i64>,
            k: Option<usize>,
            ipos: i64,
            ioff: i64,
        }
        let mut terms: Vec<Term> = Vec::new();
        {
            let ev = st.eval.as_mut().unwrap();
            for e in ev.expr.phrases() {
                let list = ev.phrase_poslist(e, icol as i32);
                let nterm = ev.ph[e].as_ref().map(|p| p.ntoken).unwrap_or(0);
                for it in 0..nterm {
                    let (l, k, ipos) = match &list {
                        Some(l) => (l.clone(), Some(0usize), l[0]),
                        None => (Vec::new(), None, 0),
                    };
                    terms.push(Term {
                        list: l,
                        k,
                        ipos,
                        ioff: (nterm - it - 1) as i64,
                    });
                }
            }
        }
        let Some(doc) = st.column_text(icol)? else {
            continue;
        };
        let toks = tokens_of(st, &doc)?;
        let mut ti = toks.into_iter();
        let mut cur = ti.next();
        if cur.is_none() {
            continue;
        }
        loop {
            let mut min = 0x7FFFFFFFi64;
            let mut which: Option<usize> = None;
            for (i, t) in terms.iter().enumerate() {
                if t.k.is_some() && t.ipos - t.ioff < min {
                    min = t.ipos - t.ioff;
                    which = Some(i);
                }
            }
            let Some(w) = which else { break };
            let t = &mut terms[w];
            let k = t.k.unwrap();
            if k + 1 >= t.list.len() {
                t.k = None;
            } else {
                t.k = Some(k + 1);
                t.ipos = t.list[k + 1];
            }
            while cur.as_ref().is_some_and(|c| (c.3 as i64) < min) {
                cur = ti.next();
            }
            match &cur {
                Some((_, s, e, _)) => {
                    res.push_str(&format!("{} {} {} {} ", icol, w, s, e - s));
                }
                None => {
                    if st.tab.content.is_none() {
                        return Err(corrupt());
                    }
                    break;
                }
            }
        }
    }
    if !res.is_empty() {
        res.pop();
    }
    Ok(res)
}

fn doctotal(st: &CursorState) -> Result<(i64, Vec<u8>)> {
    let ctx = Ctx {
        conn: &st.conn,
        tab: &st.tab,
    };
    let sql = format!("SELECT value FROM {} WHERE id=?", st.tab.shadow("stat"));
    let row = ctx.sql().row(&sql, &[Value::from_i64(0)])?;
    let a = match row.as_ref().map(|r| &r[0]) {
        Some(Value::Blob(b)) => b.to_vec(),
        _ => return Err(corrupt()),
    };
    let (ndoc, n) = get_varint(&a, 0);
    if ndoc <= 0 || n > a.len() {
        return Err(corrupt());
    }
    Ok((ndoc, a[n..].to_vec()))
}

fn mi_size(c: u8, ncol: usize, nphrase: usize) -> usize {
    match c {
        b'n' | b'p' | b'c' => 1,
        b'a' | b'l' | b's' => ncol,
        b'y' => ncol * nphrase,
        b'b' => nphrase * ncol.div_ceil(32),
        _ => ncol * nphrase * 3,
    }
}

/// `sqlite3Fts3Matchinfo`: the matchinfo blob.
pub(crate) fn matchinfo(st: &mut CursorState, fmt: Option<&[u8]>) -> Result<Vec<u8>> {
    let fmt = fmt.unwrap_or(b"pcx").to_vec();
    if st.eval.as_ref().is_none_or(|e| e.expr.root.is_none()) {
        return Ok(Vec::new());
    }
    let tab = st.tab.clone();
    let ncol = tab.ncol();
    let fmt_s = String::from_utf8_lossy(&fmt).into_owned();
    let mut global = false;
    let reuse = matches!(&st.mi, Some((f, _)) if *f == fmt_s);
    if !reuse {
        for &c in &fmt {
            let ok = matches!(c, b'p' | b'c' | b's' | b'x' | b'y' | b'b')
                || (c == b'n' && tab.fts4)
                || (c == b'a' && tab.fts4)
                || (c == b'l' && tab.has_docsize);
            if !ok {
                return Err(sql_error(format!(
                    "unrecognized matchinfo request: {}",
                    c as char
                )));
            }
        }
        global = true;
    }
    let ev = st.eval.as_ref().unwrap();
    let phrases = ev.expr.phrases();
    let nphrase = phrases.len();
    let total: usize = fmt.iter().map(|&c| mi_size(c, ncol, nphrase)).sum();
    let mut out: Vec<u32> = if reuse {
        st.mi.as_ref().unwrap().1.clone()
    } else {
        vec![0; total]
    };
    // Phrase numbers; a phrase on the right of a NOT keeps number 0.
    let mut iphrase = vec![0usize; ev.expr.nodes.len()];
    for (i, &e) in phrases.iter().enumerate() {
        iphrase[e] = i;
    }
    let mut off = 0;
    for &c in &fmt {
        let n = mi_size(c, ncol, nphrase);
        let slot = &mut out[off..off + n];
        match c {
            b'p' => {
                if global {
                    slot[0] = nphrase as u32;
                }
            }
            b'c' => {
                if global {
                    slot[0] = ncol as u32;
                }
            }
            b'n' => {
                if global {
                    slot[0] = doctotal(st)?.0 as u32;
                }
            }
            b'a' => {
                if global {
                    let (ndoc, a) = doctotal(st)?;
                    let mut p = 0;
                    for v in slot.iter_mut() {
                        let (ntok, k) = get_varint(&a, p);
                        p += k;
                        if p > a.len() {
                            return Err(corrupt());
                        }
                        *v = (((ntok as u64 & 0xffff_ffff) as i64 + ndoc / 2) / ndoc) as u32;
                    }
                }
            }
            b'l' => {
                let sql = format!("SELECT size FROM {} WHERE docid=?", tab.shadow("docsize"));
                let row = Sql::new(&st.conn).row(&sql, &[Value::from_i64(st.prev_id)])?;
                let a = match row.as_ref().map(|r| &r[0]) {
                    Some(Value::Blob(b)) => b.to_vec(),
                    _ => return Err(corrupt()),
                };
                let mut p = 0;
                for v in slot.iter_mut() {
                    let (ntok, k) = get_varint(&a, p);
                    p += k;
                    if p > a.len() {
                        return Err(corrupt());
                    }
                    *v = ntok as u32;
                }
            }
            b's' => lcs(st, slot)?,
            b'y' | b'b' => {
                slot.iter_mut().for_each(|v| *v = 0);
                let ev = st.eval.as_ref().unwrap();
                if let Some(root) = ev.expr.root {
                    lhit_gather(st, root, c, &iphrase, slot)?;
                }
            }
            _ => {
                let ev = st.eval.as_mut().unwrap();
                if global {
                    for (i, &e) in phrases.iter().enumerate() {
                        ev.phrase_stats(e, &mut slot[3 * i * ncol..3 * (i + 1) * ncol]);
                    }
                    // SQLite re-tests the row, which redoes NEAR trims.
                    ev.retest();
                }
                for (i, &e) in phrases.iter().enumerate() {
                    for col in 0..ncol {
                        let hits = ev
                            .phrase_poslist(e, col as i32)
                            .map(|l| l.len() as u32)
                            .unwrap_or(0);
                        slot[i * ncol * 3 + col * 3] = hits;
                    }
                }
            }
        }
        off += n;
    }
    if global {
        st.mi = Some((fmt_s, out.clone()));
    }
    let mut b = Vec::with_capacity(out.len() * 4);
    for v in out {
        b.extend_from_slice(&v.to_ne_bytes());
    }
    Ok(b)
}

fn lhit_gather(
    st: &CursorState,
    n: usize,
    flag: u8,
    iphrase: &[usize],
    slot: &mut [u32],
) -> Result<()> {
    let ev = st.eval.as_ref().unwrap();
    if ev.ns[n].eof || ev.ns[n].docid != ev.prev_id {
        return Ok(());
    }
    let node = &ev.expr.nodes[n];
    if let (Some(l), Some(r)) = (node.left, node.right) {
        lhit_gather(st, l, flag, iphrase, slot)?;
        lhit_gather(st, r, flag, iphrase, slot)?;
        return Ok(());
    }
    let ncol = ev.ncol;
    let picol = ev.ph[n].as_ref().map(|p| p.icol).unwrap_or(0);
    let start = if flag == b'y' {
        iphrase[n] * ncol
    } else {
        iphrase[n] * ncol.div_ceil(32)
    };
    if let Some(list) = ev.current_list(n) {
        // Column 0 is visited even when it has no positions.
        let mut cols: Vec<(i32, usize)> = Vec::new();
        if list.first().is_none_or(|(c, _)| *c != 0) {
            cols.push((0, 0));
        }
        cols.extend(list.iter().map(|(c, p)| (*c, p.len())));
        for (icol, nhit) in cols {
            if icol as usize >= ncol {
                return Err(corrupt());
            }
            if picol as usize >= ncol || picol == icol {
                if flag == b'y' {
                    slot[start + icol as usize] = nhit as u32;
                } else if nhit > 0 {
                    slot[start + icol as usize / 32] |= 1u32 << (icol & 0x1f);
                }
            }
        }
    }
    Ok(())
}

/// `fts3MatchinfoLcs`.
fn lcs(st: &mut CursorState, slot: &mut [u32]) -> Result<()> {
    let ev = st.eval.as_mut().unwrap();
    let phrases = ev.expr.phrases();
    struct It {
        e: usize,
        off: i64,
        list: Vec<i64>,
        k: usize,
        live: bool,
        ipos: i64,
    }
    let mut ntok: i64 = 0;
    let mut its: Vec<It> = phrases
        .iter()
        .map(|&e| {
            ntok -= ev.ph[e].as_ref().map(|p| p.ntoken).unwrap_or(0) as i64;
            It {
                e,
                off: ntok,
                list: Vec::new(),
                k: 0,
                live: false,
                ipos: 0,
            }
        })
        .collect();
    fn advance(it: &mut It) -> bool {
        if it.k >= it.list.len() {
            it.live = false;
            return true;
        }
        it.ipos = it.off + it.list[it.k];
        it.k += 1;
        false
    }
    for (icol, v) in slot.iter_mut().enumerate() {
        let mut nlcs = 0;
        let mut nlive = 0;
        for it in its.iter_mut() {
            match ev.phrase_poslist(it.e, icol as i32) {
                Some(l) => {
                    it.list = l;
                    it.k = 0;
                    it.live = true;
                    it.ipos = it.off;
                    advance(it);
                    nlive += 1;
                }
                None => it.live = false,
            }
        }
        while nlive > 0 {
            let mut adv: Option<usize> = None;
            let mut nthis = 0;
            for i in 0..its.len() {
                if !its[i].live {
                    nthis = 0;
                } else {
                    if adv.is_none_or(|a| its[i].ipos < its[a].ipos) {
                        adv = Some(i);
                    }
                    if nthis == 0 || its[i].ipos == its[i - 1].ipos {
                        nthis += 1;
                    } else {
                        nthis = 1;
                    }
                    if nthis > nlcs {
                        nlcs = nthis;
                    }
                }
            }
            if let Some(a) = adv {
                if advance(&mut its[a]) {
                    nlive -= 1;
                }
            }
        }
        *v = nlcs;
    }
    Ok(())
}

pub(crate) fn _err() -> LimboError {
    sql_error("")
}

pub(crate) fn _vt(v: &Value) -> Option<Vec<u8>> {
    value_text(v)
}
