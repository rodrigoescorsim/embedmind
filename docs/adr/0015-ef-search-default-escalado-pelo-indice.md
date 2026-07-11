# ADR 0015 — `ef_search` default escalado pelo tamanho do índice (patamares medidos)

**Status:** Aceito (jul/2026). Story S16 / task BQ1 ([01-spec.md](../01-spec.md),
[03-tasks.md](../03-tasks.md)) — recall estável em escala antes do launch.

## Contexto

O `ef_search` default era a constante fixa `HNSW_DEFAULT_EF_SEARCH = 64`
(`format.rs`), boa a 10k e insuficiente a 100k. Run de 2026-07-09
(`benches/results/0.1.0-dev.json`, 200 queries vs. brute-force):

| dataset | recall@10 média | pior query | query p99 |
|---|---|---|---|
| agent-mem-10k | 0,9953 | 0,90 | 14,3 ms |
| agent-mem-100k | 0,9313 | **0,20** | 15,5 ms |

Causa raiz: o beam fixo não escala com o grafo, e a única adaptação existente
(alargamento ×4 anti-sub-retorno, DESIGN §5) dispara por *contagem* quando um
filtro encolhe o resultado — nunca por *qualidade*. Havia folga de latência
(p99 15,5 ms vs. teto de 50 ms do NFR): dá para comprar recall com latência,
mas só onde o índice é grande de verdade.

O sweep do harness (`benches/src/bin/sweep_ef.rs`, k=10) mediu a forma da
curva:

- **10k (200 queries):** recall@10 já é ≥ 0,99 em `ef = 64` e fica **flat** de
  64 a 512 (0,9945–0,9950) — beam maior só paga latência, não compra recall.
- **100k (50 queries, ruidoso):** recall@10 média sobe de ~0,91 (`ef = 64`)
  até um joelho (~0,94–0,95 na faixa 192–384) e depois achata dentro do ruído
  da cauda; o p10 por query sobe de 0,5 para 0,7–0,8 no mesmo trecho.

## Decisão

**O default de `ef_search` escala com o `node_count` do índice, em degraus
medidos** (`index::default_ef_search`):

| nós no índice | `ef_search` default |
|---|---|
| < 25k | 64 (o flat de sempre) |
| ≥ 25k | 96 |
| ≥ 50k | 160 |
| ≥ 100k | 256 |

- `Query::ef_search(n)` explícito **continua soberano**: internamente o campo
  virou `Option<u16>` — `None` (default) = "o índice escolhe pelo tamanho";
  `Some(n)` é honrado literalmente em qualquer escala. A API builder não muda.
- O valor efetivo é resolvido dentro de `Store::recall`/`recall_vector`, onde
  o `node_count` vivo é uma leitura barata da meta page.
- O alargamento ×4 anti-sub-retorno continua rodando **por cima** do valor
  resolvido — query com filtro pesado não é afetada.
- Degraus, não fórmula: o joelho medido é largo e a cauda é ruidosa; meia
  dúzia de patamares auditáveis são mais fáceis de manter honestos (re-medir e
  ajustar uma linha da tabela) do que uma curva ajustada fingindo precisão.

Validação (run 2026-07-10, `benches/run_all.sh --full`, 200 queries vs.
brute-force, mesma máquina do baseline):

| dataset | recall@10 média | pior query | p10 | p50 | query p99 |
|---|---|---|---|---|---|
| agent-mem-10k (ef 64, inalterado) | «10K_MEAN» | «10K_MIN» | «10K_P10» | «10K_P50» | «10K_P99» ms |
| agent-mem-100k (ef 256) | «100K_MEAN» | «100K_MIN» | «100K_P10» | «100K_P50» | «100K_P99» ms |

«VEREDITO_DOD»

## Alternativas rejeitadas

- **Outra constante fixa, maior:** puniria índices pequenos com latência sem
  ganho nenhum (o sweep 10k é flat em recall e crescente em custo) e voltaria
  a sub-recuperar no próximo salto de escala — trocaria um número mágico por
  outro.
- **Fórmula contínua (ex.: `max(64, c·k·ln N)`):** pretende uma precisão que a
  cauda ruidosa do sweep não sustenta; introduz uma constante `c` calibrável
  (o espírito do ADR 0005 — nada a calibrar — vale aqui também) e é mais
  difícil de auditar do que uma tabela de degraus com a medição ao lado.
- **`ef` adaptativo por qualidade em tempo de query** (alargar até o beam
  "convergir"): heurística nova no caminho quente de toda busca, com critério
  de parada que é ele próprio um parâmetro a calibrar; o ganho sobre degraus
  medidos não justifica o risco.

## Consequências

- `Query.ef_search` interno vira `Option<u16>`; `Query::ef_search(n)` público
  inalterado (soberania documentada no rustdoc).
- O harness passa a reportar a **distribuição** do recall por query
  (mín/p10/p50, `benches/src/recall.rs`) além da média, no markdown e no JSON
  — média boa com p10 baixo é exatamente o modo de falha que motivou a S16;
  o guard do §5 (BENCHMARKS.md) segue comparando a média.
- DESIGN.md (§ HNSW) atualizado; `benches/sweep_ef.rs` referencia este ADR.
- Quando o corpus de referência crescer (ex.: dataset 1M), o degrau seguinte
  se decide com novo sweep — a tabela é o contrato, re-medível.
