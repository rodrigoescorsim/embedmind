# ADR 0015 — `ef_search` default escalado pelo tamanho do índice (patamares medidos)

**Status:** Aceito (jul/2026) — o mecanismo de degraus fica; **DoD da story S16
reprovado** na validação de 2026-07-11 (ver seção "Validação" abaixo). Story
S16 / task BQ1 ([01-spec.md](../01-spec.md), [03-tasks.md](../03-tasks.md)) —
recall estável em escala antes do launch.

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

Validação (run 2026-07-11, `benches/run_all.sh --full`, 1000 queries vs.
brute-force via `Store::recall_vector` — a metade HNSW pura, não contaminada
pela busca híbrida — mesma máquina do baseline; log completo em
`benches/results/adr0014-off.log`):

| dataset | recall@10 média | pior query | p10 | p50 | query p99 (híbrido, e2e) |
|---|---|---|---|---|---|
| agent-mem-10k (ef 64, inalterado) | 0,9953 | 0,90 | 1,00 | 1,00 | 103,09 ms |
| agent-mem-100k (ef 256) | 0,9360 | 0,20 | 0,70 | 1,00 | 1224,62 ms |

**Reprovado — parcialmente.** O default escalado por degraus melhora o
recall@10 médio a 100k (0,9313 → 0,9360) e o p10 por query (a cauda "razoável"
sobe: 0,70 no lugar de queries piores), mas **não** atinge o DoD original da
S16 nos dois eixos que ele exige:

- **recall@10 média @ 100k:** 0,9360 < 0,95 alvo. **Reprova.**
- **pior query @ 100k:** 0,20 < 0,70 alvo — idêntico ao número que motivou a
  story (`docs/03-tasks.md` BQ1), apesar do degrau `ef=256`. Com 1000 queries
  (a rodada real do harness, não as 50 ruidosas do sweep) a cauda de pior caso
  não desaparece: existe pelo menos uma consulta cujo vizinho verdadeiro fica
  fora do beam mesmo em `ef=256`. **Reprova.**
- **query p99 < 50 ms:** 1224,62 ms medido (híbrido, e2e) a 100k — **23x o
  teto**. Causa raiz **não é o `ef_search`**: o vetor puro decidido por
  `default_ef_search` é rápido (ver `↳ query engine p50/p99` no relatório
  completo; o HNSW em si não é o gargalo). O custo dominante é a busca
  full-text: o índice de postings do FTS decodifica a lista inteira por termo
  a cada query (decisão pré-existente, fora do escopo deste ADR — não há
  paginação/skip-list nas postings), e cresce linearmente com o corpus. A 100k
  isso já domina o p99 do `recall` híbrido. Achado novo, registrado aqui para
  não se perder: **fora de escopo da S16**, precisa de story própria
  (ver "Consequências").

O 10k segue saudável (recall 0,9953 média / 0,90 pior, degrau inalterado —
sem regressão além do limiar §5 do BENCHMARKS.md). O `ef_search` escalado por
si só está correto e ativo (`index::default_ef_search`, testado por
`default_ef_search_scales_up_with_node_count_and_is_monotonic` e cobertura de
`Query::effective_ef_search`); o que falha é o DoD do S16 como um todo, por
dois motivos: a cauda de pior-caso do HNSW a 100k não fecha em 0,70 mesmo no
maior degrau medido, e o p99 é dominado por um gargalo do FTS que este ADR
nunca teve como escopo consertar.

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
- **RSS de pico @ 100k também estoura o NFR** (< 300 MiB): medido 307,1 MiB
  (query) / 305,4 MiB (ingest) na mesma rodada — a folga de 6% citada na task
  BQ1 (280,9 MiB) já não existe. Reportado aqui e no CHANGELOG por
  transparência; a causa é dimensionamento geral do índice a 100k, não algo
  que o `ef_search` escalado piora sozinho (o degrau maior consome mais RAM
  durante a busca, mas a folga já estava apertada antes deste ADR). Sem
  correção nesta task — precisa de investigação própria.
- **Duas reprovações do DoD original ficam registradas como dívida técnica,
  não escondidas:**
  1. **Recall de pior-caso a 100k não fecha em 0,70** mesmo no degrau máximo
     medido (`ef=256`); subir o degrau é a alavanca óbvia, mas o sweep já
     mostrou a curva achatando por volta de 192–384 — o próximo passo exige
     ou um degrau ainda maior com nova medição de custo, ou revisitar a
     construção do índice (`ef_construction`, `M`) em vez de só o lado da
     busca.
  2. **`query p99 < 50 ms` (híbrido) falha por um gargalo pré-existente do
     FTS**, não do HNSW: a lista de postings é decodificada inteira por termo
     a cada query, sem paginação — custo linear no tamanho do corpus,
     dominante a 100k (p99 híbrido 1224,62 ms vs. HNSW puro, que é rápido).
     Fora do escopo deste ADR e da story S16; precisa de story própria no
     roadmap (índice de postings com skip/paginação) antes do NFR de latência
     poder ser revalidado a 100k.
