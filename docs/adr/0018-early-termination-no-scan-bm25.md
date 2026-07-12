# ADR 0018 — Early termination no scan BM25 por avaliação limitada por bound (FT2)

**Status:** Aceito (jul/2026). Executa o passo 1 da ordem de risco do
[ADR 0017](0017-otimizacao-do-full-text-escopo-e-metodo.md) §3 — mudança de
algoritmo de scan, zero mudança de formato (`format_version` continua 3).

## Contexto

O profiling da FT1 (ADR 0017 §"Resultado do profiling",
[`benches/results/profile-fts-100k.txt`](../../benches/results/profile-fts-100k.txt))
mediu onde o meio full-text do `recall` híbrido gasta o tempo @ 100k:

| fase | fração |
|---|---:|
| postings lookup (I/O de página + decode) | 1,2% |
| **`keep` (recarga do registro + re-checagem)** | **88,8%** |
| `doc_len` (recarga + re-tokenização) | 4,5% |
| scoring (`HashMap` + sort) | 5,5% |

A hipótese original da story S25 (cortar a *decodificação* das postings) foi
**redirecionada pela medição**: decodificar custa 1,2%; o que domina é a
avaliação por candidato — `keep` + `doc_len` recarregam o registro inteiro do
B-tree para cada id distinto que o scan encontra (~todo o corpus numa query
com termos comuns), somando 93,3% do tempo. O corte que paga é, portanto,
**parar de avaliar candidatos**, não parar de decodificar bytes.

## Decisão

`fts::search` passa a fazer o scan em duas passadas (implementação em
`crates/embedmind-core/src/index/fts.rs::search`):

1. **Passada de bounds (barata, sem closures):** decodifica as postings de
   cada termo casado uma única vez (1,2% do tempo medido) e acumula, por
   candidato, um **upper bound** do seu score BM25: a contribuição exata
   avaliada em `dl = 0`, que minimiza a normalização de comprimento e
   portanto domina a contribuição real para qualquer comprimento de
   documento. Nenhum `keep`/`doc_len` é chamado aqui.
2. **Passada de avaliação (cara, cortada cedo):** ordena os candidatos por
   bound decrescente (empate por id, determinístico) e os avalia exatamente
   — `keep`, `doc_len`, score BM25 completo — nessa ordem, mantendo os `k`
   melhores. **Critério de corte: quando já existem `k` hits exatos e o
   próximo bound é estritamente menor que o k-ésimo melhor score exato, o
   scan para** — todo candidato não avaliado tem score real ≤ seu bound,
   logo não entra no top-k nem desloca empate (empate exigiria bound igual
   ao k-ésimo score, que não é estritamente menor).

### Por que este critério (decidido por medição, não a priori)

- **Limiar de score (o escolhido)** é o único dos critérios listados na
  story ("limiar de score, contagem de candidatos ou heurística
  equivalente") que preserva a invariante dura de resultado idêntico:
  contagem fixa de candidatos ou heurística de saturação podem parar antes
  de um candidato tardio com score real alto — quebram a identidade.
- A medição da FT1 mostra que o custo é **por candidato avaliado** (recarga
  de registro), não por byte decodificado — então o corte precisa reduzir
  *avaliações*, e o bound por candidato é o que permite pular avaliações com
  prova de segurança.

### Por que o resultado é idêntico (mesmos documentos, mesma ordem, mesmos scores)

- O score exato soma as contribuições **na mesma ordem de termos** (termos
  da query ordenados/deduplicados) e **com a mesma expressão f32** do scan
  exaustivo → scores bit-idênticos.
- O bound domina o score real também em f32: `norm` cresce monotonicamente
  com `dl` e `+`, `×`, `÷` arredondam monotonicamente em IEEE 754; em
  `dl = 0` bound e score exato são a mesma expressão, bit a bit.
- Desempate por `(score desc, id asc)` nas duas versões (G3); o corte
  estrito nunca descarta um empate possível.
- `search_profiled` (FT1) permanece com o scan exaustivo pré-FT2 e vira o
  **oráculo de equivalência**: teste
  `early_termination_matches_exhaustive_scan_on_larger_corpus` (corpus com
  corte ativo, filtro `keep`, empates exatos e registros sumidos, k de 1 a
  500) + `search_profiled_matches_search_exactly` (corpus pequeno) +
  checagem no corpus real @ 100k (`bench_fts`, 25 queries, ids + scores
  bit-exatos + ordem: 25/25 idênticas).

Refinamento de semântica registrado: um erro de filtro de metadados (tipo
incompatível) agora só dispara se o registro ofensor for *avaliado* — o scan
exaustivo o disparava para qualquer registro que compartilhasse um termo com
a query, mesmo sem chance de entrar no top-k. Os *resultados* retornados nos
casos sem erro são idênticos.

## Resultado (antes/depois @ 100k, 1000 queries, warm cache)

Baseline pré-FT2 preservado (não sobrescrito): FT1
`benches/results/profile-fts-100k.txt`; harness `query_engine_p99_ms`
1.224,62 ms (ADR 0015). Depois:
[`benches/results/bench-fts-100k-after-ft2.txt`](../../benches/results/bench-fts-100k-after-ft2.txt).

| medida (meio full-text, sem embed) | antes (FT1, scan exaustivo) | depois (FT2, bounded) | ganho |
|---|---:|---:|---:|
| p50 | 994,33 ms | **94,50 ms** | 10,5x |
| p99 | 4.576,46 ms | **550,78 ms** | 8,3x |

(O "antes" mede `search_text_profiled`, que inclui overhead de
instrumentação `Instant` por posting; a ordem de grandeza confere com o
harness sem instrumentação — 1.215–1.224 ms de p99 engine.)

**O NFR `recall p99 @ 100k < 50 ms` ainda não fecha** — o corte elimina as
avaliações redundantes, mas a passada de bounds continua O(total de postings
dos termos da query) (~366 mil/query) e o k-ésimo score só estabiliza depois
de avaliar candidatos reais. A FT3 (delta+varint/skip lists, ADR 0017 §3
passos 2–3) segue necessária. Re-rodada completa de `benches/run_all.sh
--full` para atualizar `query_engine_p50/p99_ms` oficiais fica para a
validação da fase (ADR 0017 §4).

## Alternativas rejeitadas

- **WAND/MaxScore DAAT completo** (travessia paralela das listas com pivô):
  mesma exatidão, mais complexidade, e só supera a versão em duas passadas
  se evitar decodificar postings — o que a 1,2% de custo não paga hoje;
  reavaliar na FT3, onde skip lists tornam o salto em disco possível.
- **Corte por contagem de candidatos / saturação de score**: quebra a
  invariante de resultado idêntico (ver acima).
- **Persistir `doc_len`/flags de liveness para baratear a avaliação**: é o
  passo 4 do ADR 0017 §3, reabre o trade-off do ADR 0011 ("um dado a menos
  que pode divergir em disco"); só entra se FT3 não fechar o NFR.
