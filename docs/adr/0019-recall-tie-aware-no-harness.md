# ADR 0019 — Recall@k tie-aware no harness: paridade de score contra o top-k exato (FT4/S27)

**Status:** Aceito (jul/2026). Fecha a investigação da story S27 (task FT4) —
mudança **só no harness de benchmark** (`benches/`), zero mudança na engine,
no formato (`format_version` continua 3) ou em qualquer parâmetro HNSW.

## Contexto

O ADR 0015 deixou a pior query do lote de 1000 do harness em recall@10 = 0,20
@ 100k contra o alvo ≥ 0,70, mesmo no degrau máximo medido (`ef_search = 256`),
e a média em 0,9360 contra o alvo ≥ 0,95. A S27 mandava investigar, sem
escolher a priori: (a) `ef_construction`/`M` na construção do índice; (b) um
degrau de `ef_search` > 256; (c) heurística de retry para queries de baixa
confiança.

Antes de escolher entre os três, um probe de diagnóstico
(`benches/src/bin/probe_worst.rs`) respondeu a pergunta anterior: **que tipo
de miss é a cauda?** Para cada query do harness ele nota o top-10 do HNSW de
duas formas contra o top-10 exato do scan bruto:

- **id overlap** — a métrica vigente: dos 10 *ids* exatos, quantos o HNSW
  devolveu?
- **paridade de score (tie-aware)** — dos hits devolvidos, quantos têm score
  de cosseno exato que empata (± `SCORE_TIE_EPS = 1e-5`) ou supera o 10º
  score exato? É o grading estilo ann-benchmarks.

### Resultado da medição (2026-07-12, 1000 queries, ef default do harness)

Saídas brutas: [`benches/results/probe-worst-100k.txt`](../../benches/results/probe-worst-100k.txt)
e [`probe-worst-10k.txt`](../../benches/results/probe-worst-10k.txt).

| dataset | grading | média | mín | p10 | p50 |
|---|---|---:|---:|---:|---:|
| agent-mem-100k | id overlap (métrica antiga) | 0,9360 | 0,20 | 0,70 | 1,00 |
| agent-mem-100k | **paridade de score** | **1,0000** | **1,00** | 1,00 | 1,00 |
| agent-mem-10k | id overlap (métrica antiga) | 0,9953 | 0,90 | 1,00 | 1,00 |
| agent-mem-10k | **paridade de score** | **1,0000** | **1,00** | 1,00 | 1,00 |

(A linha de id overlap reproduz exatamente os números do harness/ADR 0015 —
mesmas queries, mesmas condições.)

O corpus sintético contém **textos duplicados exatos por construção** (mesmo
template + mesmos slots → texto byte-idêntico): 8,4% no 10k, **23,0% no
100k**. Texto idêntico embeda em vetor bit-idêntico, então a fronteira do
top-10 exato é rotineiramente um **platô de scores empatados mais largo que
k**: nas 70 queries abaixo de 0,70 de id overlap @ 100k, o platô tem 14–29
vetores empatando o 10º score. *Quais* ids empatados um índice correto devolve
é arbitrário — e **todas as 70 têm paridade de score 1,00**: a cauda é 100%
artefato de empate, não miss do HNSW. A métrica antiga media o desempate, não
o índice.

A escada de `ef` do probe (384/512/1024/2048, por query ruim) confirma pelo
outro lado: o id overlap **não melhora monotonicamente com `ef`** (ex.: query
672: 0,20 → ef 384: 0,90 → ef 512: 0,20) porque é sorteio dentro do platô, e o
custo por query vai de ~50 ms a ~660 ms — nenhum parâmetro de busca ou de
construção conserta uma métrica que pune cara-ou-coroa entre vizinhos
igualmente próximos.

## Decisão

O grading do recall@k do harness passa a ser **tie-aware (paridade de
score)**: um hit devolvido conta quando seu score de cosseno exato empata
(dentro de `SCORE_TIE_EPS = 1e-5`) ou supera o k-ésimo score exato do
baseline, com o overlap limitado a k (platô mais largo que k nunca nota acima
de 1,0). Implementação:

- `benches/src/baseline.rs` — `SCORE_TIE_EPS`, `tie_aware_overlap`,
  `tie_aware_recall_by_position`;
- `benches/src/recall.rs` — `measure` re-pontua cada hit do EmbedMind contra o
  vetor da query e nota por paridade;
- `benches/src/competitors.rs` — sqlite-vec, zvec e Chroma notados pela
  **mesma régua** (ninguém ganha nem perde pelo sorteio de empates).

`SCORE_TIE_EPS = 1e-5` absorve só ruído de ordem de soma f32 (duplicatas
exatas têm delta 0,0; o ruído observado é ~1e-6, ex. k-ésimos scores
1,000001/0,999999); medido no probe, o grading é idêntico com 1e-5 e 1e-4 nos
dois datasets.

Nenhuma mudança na engine: (a), (b) e (c) da story foram **descartados pelo
dado** — não há miss real a consertar (paridade 1,00 em 2000/2000 queries nos
dois datasets).

## Antes/depois (harness @ ef default, 1000 queries)

| dataset | métrica | antes (id overlap) | depois (tie-aware) | alvo |
|---|---|---:|---:|---|
| agent-mem-100k | recall@10 média | 0,9360 | **1,0000** | ≥ 0,95 ✅ |
| agent-mem-100k | pior query | 0,20 | **1,00** | ≥ 0,70 ✅ |
| agent-mem-100k | p10 / p50 | 0,70 / 1,00 | 1,00 / 1,00 | sem regressão ✅ |
| agent-mem-10k | recall@10 média | 0,9953 | **1,0000** | — |
| agent-mem-10k | pior query | 0,90 | **1,00** | — |

Latência, RSS e tamanho de arquivo intocados: a mudança é de *medição* (a
re-pontuação acontece fora das seções cronometradas do harness) e nada na
engine mudou.

## Alternativas rejeitadas

- **(a) Revisitar `ef_construction`/`M` na construção:** rebuild completo do
  índice de 100k para, no máximo, embaralhar *quais* ids empatados aparecem —
  a paridade de score já é 1,00, não há qualidade de grafo a recuperar.
- **(b) Degrau de `ef_search` > 256:** medido 384–2048; id overlap oscila de
  forma não monotônica (sorteio de empate) e o custo por query chega a
  ~660 ms @ ef 2048 — orçamento de p99 que não existe (FT1–FT3). Descartado.
- **(c) Retry/expansão para baixa confiança:** não existe sinal de baixa
  confiança para disparar — as queries "ruins" têm o k-ésimo score exato
  altíssimo (frequentemente 1,0: dezenas de matches exatos), o oposto de
  baixa confiança.
- **Remover duplicatas do corpus em vez de mudar o grading:** duplicatas são
  realistas para memória de agente (o mesmo fato re-lembrado) e os datasets
  são versionados — reescrevê-los invalidaria silenciosamente todo o
  histórico de números. O grading tie-aware é a correção padrão da literatura
  (ann-benchmarks) para o fenômeno.

## Consequências

- O NFR de recall médio da S16 (≥ 0,95 @ 100k), reprovado no ADR 0015 pela
  mesma raiz, passa a fechar junto.
- **Limitação honesta:** com grading tie-aware os dois datasets comprometidos
  medem 1,0000 — na fronteira do top-10 eles deixam de discriminar qualidade
  de índice (os platôs de duplicatas cobrem qualquer aproximação do HNSW no
  ef default). O guarda de regressão (§5 do BENCHMARKS.md) continua válido —
  uma regressão real de grafo derrubaria a paridade — mas um corte fino de
  qualidade exigiria o dataset público rotulado (BENCHMARKS.md §2) ou uma
  revisão versionada do gerador com taxa de duplicatas controlada; decisão do
  founder, fora desta story.
- O baseline rolling do CI (§5) foi gerado com o grading antigo; o primeiro
  run com o novo só pode subir o recall@10 (tie-aware ≥ id overlap por
  construção), então o guarda não dispara e o baseline novo absorve o degrau.
- `probe_worst` permanece no harness como ferramenta de diagnóstico da cauda
  (dupla notação + censo de duplicatas + escada de ef por query ruim).
