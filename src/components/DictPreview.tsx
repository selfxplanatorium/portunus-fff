import { useState, useEffect } from 'react';
import { invoke } from '@tauri-apps/api/core';
import type { PreviewProps } from '../providers/registry';
import { CopyIcon, CheckIcon } from '../icons';

interface DictDefinition {
  pos: string;
  num: number;
  text: string;
  quotes: string[];
  synonyms: string[];
}

interface DictResult {
  word: string;
  definitions: DictDefinition[];
}

const POS_PALETTE: Record<string, { bg: string; fg: string }> = {
  Noun:      { bg: 'rgba(141,188,212,0.12)', fg: '#8dbcd4' },
  Verb:      { bg: 'rgba(150,190,101,0.12)', fg: '#96be65' },
  Adjective: { bg: 'rgba(232,192,122,0.12)', fg: '#e8c07a' },
  Adverb:    { bg: 'rgba(200,168,224,0.12)', fg: '#c8a8e0' },
};

const STYLES = `
.dict-preview {
  padding: 16px 18px;
  display: flex;
  flex-direction: column;
  gap: 11px;
  flex: 1;
  min-height: 0;
}

.dict-header {
  display: flex;
  align-items: baseline;
  justify-content: space-between;
  gap: 8px;
  flex-shrink: 0;
  padding-bottom: 10px;
  border-bottom: 1px solid var(--line);
}

.dict-word {
  font-size: 21px;
  font-weight: 700;
  color: var(--fg);
  letter-spacing: -0.03em;
  line-height: 1;
  min-width: 0;
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
}

.dict-header-right {
  display: flex;
  align-items: center;
  gap: 7px;
  flex-shrink: 0;
}

.dict-count {
  font: 400 10px/1 var(--font-mono);
  color: var(--fg-dim);
  white-space: nowrap;
}

.dict-list {
  flex: 1;
  min-height: 0;
  overflow-y: auto;
  animation: dict-list-in 0.18s ease both;
}
.dict-list::-webkit-scrollbar { width: 4px; }
.dict-list::-webkit-scrollbar-thumb { background: var(--bg-input); border-radius: 2px; }
.dict-list::-webkit-scrollbar-track { background: transparent; }

@keyframes dict-list-in {
  from { opacity: 0; }
  to   { opacity: 1; }
}

.dict-def {
  padding: 9px 0;
  border-bottom: 1px solid var(--line-soft);
  display: flex;
  flex-direction: column;
  gap: 4px;
}
.dict-def:last-child { border-bottom: none; padding-bottom: 4px; }

.dict-def-top {
  display: flex;
  align-items: flex-start;
  gap: 7px;
}

.dict-num {
  font: 600 10px/1.7 var(--font-mono);
  color: var(--fg-dim);
  min-width: 12px;
  flex-shrink: 0;
  text-align: right;
}

.dict-def-content { flex: 1; min-width: 0; }

.dict-pos {
  display: inline-block;
  padding: 1px 5px 2px;
  border-radius: 3px;
  font: 600 8px/1.5 var(--font-mono);
  letter-spacing: 0.08em;
  text-transform: uppercase;
  margin-right: 5px;
  vertical-align: middle;
  position: relative;
  top: -1px;
}

.dict-text {
  font-size: 13.5px;
  color: var(--fg);
  line-height: 1.55;
}

.dict-quotes {
  margin-left: 19px;
  margin-top: 2px;
  padding-left: 8px;
  border-left: 1px solid var(--accent-border);
  display: flex;
  flex-direction: column;
  gap: 2px;
}

.dict-quote {
  font-size: 12.5px;
  font-style: italic;
  color: var(--quote-fg);
  line-height: 1.45;
}
.dict-quote::before { content: '"'; margin-right: 1px; }
.dict-quote::after  { content: '"'; margin-left: 1px; }

.dict-syns {
  margin-left: 19px;
  margin-top: 3px;
  display: flex;
  flex-wrap: wrap;
  gap: 3px;
}

.dict-syn {
  font: 400 10.5px/1 var(--font-mono);
  color: var(--fg-dim);
  background: var(--bg-footer);
  border: 1px solid var(--line-soft);
  padding: 2px 5px;
  border-radius: 3px;
  white-space: nowrap;
}

/* Skeleton */
.dict-skel { flex: 1; display: flex; flex-direction: column; gap: 10px; padding-top: 2px; }

.dict-skel-row {
  border-radius: 4px;
  background: var(--bg-skeleton);
  position: relative;
  overflow: hidden;
  flex-shrink: 0;
}
.dict-skel-row::before {
  content: '';
  position: absolute;
  inset: 0;
  background: linear-gradient(110deg, transparent 30%, rgba(255,255,255,0.04) 50%, transparent 70%);
  background-size: 200% 100%;
  animation: pdf-shimmer 1.6s linear infinite;
}

/* Error */
.dict-error {
  flex: 1;
  display: flex;
  flex-direction: column;
  align-items: center;
  justify-content: center;
  gap: 7px;
  color: var(--fg-dim);
  font: 400 12px/1.5 var(--font-mono);
}
.dict-error-glyph {
  display: flex;
  opacity: 0.32;
  margin-bottom: 4px;
}

/* Hint */
.dict-hint {
  padding: 20px 20px;
  display: flex;
  flex-direction: column;
  gap: 14px;
  flex: 1;
}
.dict-hint-hero {
  display: flex;
  align-items: center;
  gap: 14px;
}
.dict-hint-name {
  font-size: 15px;
  font-weight: 600;
  color: var(--fg);
  letter-spacing: -0.01em;
}
.dict-hint-desc {
  margin-top: 4px;
  font-size: 12px;
  color: var(--fg-mute);
  line-height: 1.55;
}
.dict-hint-divider {
  height: 1px;
  background: var(--line-soft);
}
.dict-hint-prose {
  font-size: 12px;
  color: var(--fg-mute);
  line-height: 1.7;
}
.dict-hint-token {
  font: 500 11.5px/1 var(--font-mono);
  color: var(--accent);
  background: var(--accent-soft);
  padding: 2px 5px;
  border-radius: 3px;
}
.dict-hint-example {
  display: flex;
  align-items: baseline;
  gap: 7px;
  padding: 9px 12px;
  background: var(--bg-deep);
  border-radius: var(--radius-sm);
  border-left: 2px solid var(--accent);
}
.dict-hint-ex-cmd {
  font: 600 13px/1 var(--font-mono);
  color: var(--accent);
}
.dict-hint-ex-word {
  font: 400 13px/1 var(--font-mono);
  color: var(--fg-mute);
}
`;

if (typeof document !== 'undefined') {
  const id = 'dict-preview-styles';
  if (!document.getElementById(id)) {
    const el = document.createElement('style');
    el.id = id;
    el.textContent = STYLES;
    document.head.appendChild(el);
  }
}

export const dictCache = new Map<string, DictResult | null>();

function posStyle(pos: string): { bg: string; fg: string } {
  return POS_PALETTE[pos] ?? { bg: 'var(--accent-soft)', fg: 'var(--accent)' };
}

export default function DictPreview({ result }: PreviewProps) {
  if (result.kind === 'dict-hint') return <HintPanel />;
  return <DefinitionView word={result.title} />;
}

function HintPanel() {
  return (
    <div className="dict-hint">
      <div className="dict-hint-hero">
        <div className="file-preview-icon-wrap">
          <BookIcon />
        </div>
        <div>
          <div className="dict-hint-name">WordNet Dictionary</div>
          <div className="dict-hint-desc">
            Definitions, synonyms, and usage examples for any English word.
          </div>
        </div>
      </div>

      <div className="dict-hint-divider" />

      <div className="dict-hint-prose">
        Type{' '}
        <span className="dict-hint-token">define</span>
        {' '}or{' '}
        <span className="dict-hint-token">dict</span>
        {' '}followed by any word:
      </div>

      <div className="dict-hint-example">
        <span className="dict-hint-ex-cmd">define</span>
        <span className="dict-hint-ex-word">serendipity</span>
      </div>
    </div>
  );
}

function DefinitionView({ word }: { word: string }) {
  const [data, setData] = useState<DictResult | null>(null);
  const [error, setError] = useState(false);
  const [loading, setLoading] = useState(true);
  const [copied, setCopied] = useState(false);

  useEffect(() => {
    if (dictCache.has(word)) {
      const cached = dictCache.get(word) ?? null;
      if (cached) {
        setData(cached);
        setError(false);
      } else {
        setError(true);
      }
      setLoading(false);
      return;
    }
    let cancelled = false;
    setData(null);
    setError(false);
    setLoading(true);
    // Debounce: don't fire the (online) lookup for words we just scroll past.
    // Only the word we settle on for ~180ms is fetched.
    const timer = setTimeout(() => {
      invoke<DictResult>('get_dict_definitions', { word })
        .then(r => {
          // Cache regardless of navigation so returning to this word is instant.
          dictCache.set(word, r);
          if (!cancelled) {
            setData(r);
            setLoading(false);
          }
        })
        .catch(() => {
          dictCache.set(word, null);
          if (!cancelled) {
            setError(true);
            setLoading(false);
          }
        });
    }, 180);
    return () => { cancelled = true; clearTimeout(timer); };
  }, [word]);

  const copyFirst = () => {
    if (data?.definitions[0]) {
      navigator.clipboard.writeText(data.definitions[0].text);
      setCopied(true);
      setTimeout(() => setCopied(false), 1500);
    }
  };

  if (!loading && error) {
    return (
      <div className="dict-preview">
        <div className="dict-error">
          <span className="dict-error-glyph"><NotFoundIcon /></span>
          <span>No definitions found for <strong style={{ color: 'var(--fg-mute)' }}>{word}</strong></span>
        </div>
      </div>
    );
  }

  return (
    <div className="dict-preview">
      <div className="dict-header">
        <div className="dict-word">{word}</div>
        {data && !loading && (
          <div className="dict-header-right">
            <span className="dict-count">
              {data.definitions.length} def{data.definitions.length !== 1 ? 's' : ''}
            </span>
            <button className={`file-btn-icon${copied ? ' copied' : ''}`} onClick={copyFirst} tabIndex={-1}>
              {copied ? <CheckIcon /> : <CopyIcon />}
            </button>
          </div>
        )}
      </div>

      {loading || !data ? (
        <div className="dict-skel">
          <div className="dict-skel-row" style={{ height: 38 }} />
          <div className="dict-skel-row" style={{ height: 54 }} />
          <div className="dict-skel-row" style={{ height: 44 }} />
        </div>
      ) : (
        <div className="dict-list">
          {data.definitions.map((def, i) => {
            const { bg, fg } = posStyle(def.pos);
            return (
              <div key={i} className="dict-def">
                <div className="dict-def-top">
                  <span className="dict-num">{def.num}</span>
                  <div className="dict-def-content">
                    <span className="dict-pos" style={{ background: bg, color: fg }}>
                      {def.pos}
                    </span>
                    <span className="dict-text">{def.text}</span>
                  </div>
                </div>

                {def.quotes.length > 0 && (
                  <div className="dict-quotes">
                    {def.quotes.map((q, qi) => (
                      <span key={qi} className="dict-quote">{q}</span>
                    ))}
                  </div>
                )}

                {def.synonyms.length > 0 && (
                  <div className="dict-syns">
                    {def.synonyms.slice(0, 8).map((syn, si) => (
                      <span key={si} className="dict-syn">{syn}</span>
                    ))}
                    {def.synonyms.length > 8 && (
                      <span className="dict-syn" style={{ opacity: 0.6 }}>
                        +{def.synonyms.length - 8}
                      </span>
                    )}
                  </div>
                )}
              </div>
            );
          })}
        </div>
      )}
    </div>
  );
}

function NotFoundIcon() {
  return (
    <svg width="38" height="38" viewBox="0 0 24 24" fill="none"
      stroke="currentColor" strokeWidth="1.5" strokeLinecap="round" strokeLinejoin="round">
      <circle cx="10.5" cy="10.5" r="7.5" />
      <line x1="21" y1="21" x2="15.8" y2="15.8" />
      <line x1="7.5" y1="10.5" x2="13.5" y2="10.5" />
    </svg>
  );
}

function BookIcon() {
  return (
    <svg width="20" height="20" viewBox="0 0 24 24" fill="none"
      stroke="currentColor" strokeWidth="1.8" strokeLinecap="round" strokeLinejoin="round">
      <path d="M4 19.5A2.5 2.5 0 0 1 6.5 17H20" />
      <path d="M6.5 2H20v20H6.5A2.5 2.5 0 0 1 4 19.5v-15A2.5 2.5 0 0 1 6.5 2z" />
      <line x1="10" y1="7" x2="16" y2="7" />
      <line x1="10" y1="11" x2="16" y2="11" />
    </svg>
  );
}
