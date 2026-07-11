# ADR 0014 — Recência como terceira lista na fusão RRF do recall

**Status:** Aceito (jul/2026). Story S20 / fase FR2 ([01-spec.md](../01-spec.md),
[03-tasks.md](../03-tasks.md)) — "frescor do conhecimento": empate semântico
pende para o mais novo sem derrubar match forte antigo.

## Contexto

Um agente que corrige um fato grava uma memória nova; a antiga pode continuar
viva (sem `supersedes`). Num `recall` posterior, as duas são igualmente
relevantes em conteúdo — e o desejável é a correção vir primeiro. A restrição
herdada do ADR 0005: **nada a calibrar** — a fusão híbrida usa só posições de
rank (RRF k=60), nunca escalas de score, e qualquer mecanismo de recência tem
que preservar isso. A pergunta de design: **onde a recência entra sem virar um
peso ajustável e sem trazer item irrelevante só por ser novo?**

## Decisão

**Recência é uma terceira lista na mesma fusão RRF k=60 — os MESMOS candidatos
de conteúdo (união vetor+texto), reordenados por `created_at_micros`
decrescente.** Nenhum score novo, nenhuma constante nova: só mais uma lista de
posições de rank, somada com as outras duas por `1/(k + rank + 1)`.

- **Nunca traz item de fora.** A lista é construída da união dos candidatos
  que as buscas de conteúdo já retornaram — cada um já aprovado pela mesma
  closure `keep` (escopo, tombstone, `superseded` do FR1, filtros de
  metadados, agente). Um excluído não reentra por ser novo; um irrelevante não
  entra por ser novo.
- **Nunca inverte um domínio de conteúdo.** Propriedade do próprio RRF: a
  contribuição máxima de uma lista é `1/(k+1)`. Um match rank 0 em vetor E
  texto (duas contribuições de conteúdo) sempre supera um candidato que só a
  recência favorece — ele só perde para outro hit de conteúdo comparável,
  exatamente a borda da spec S20.
- **`fuse` generalizado para N listas** (`recall::fuse_lists`), com o mesmo
  determinismo total: empate de score fundido quebra pela primeira
  `(índice da lista, rank)` em que o id apareceu — property tests cobrem a
  fusão de N listas; o `fuse` de 2 listas vira um wrapper.
- **Default: desligado (opt-in)** — decidido por medição (abaixo), como a spec
  S20 manda. `Query::recency(bool)` no core, `recency` (boolean) no MCP e
  `--recency` no CLI dão controle explícito nos dois sentidos.

## Medição (harness `benches/run_all.sh --full`, 2026-07-10)

A spec condiciona o default ao recall@10 vs. brute-force do harness
(limiar §5 do BENCHMARKS.md). Duas observações medidas:

1. **recall@10 vs. brute-force é estruturalmente insensível à recência**: a
   métrica compara o índice vetorial (`recall_vector`) com o brute-force
   exato — a recência não toca a busca vetorial, só a fusão do `recall`
   híbrido. Medido para confirmar: idêntico com e sem recência
   (10k 0.9953 · 100k 0.9360, bit a bit iguais nas duas execuções).
2. **Latência do `recall` híbrido** (o custo real da lista extra — recarregar
   `created_at` dos ≤ 2·limit candidatos e ordenar):

| dataset | query p99 sem recência | query p99 com recência | Δ |
|---|---|---|---|
| agent-mem-10k | 103.09 ms | 103.13 ms | +0.04 ms (+0.04%) |
| agent-mem-100k | 1224.62 ms | 2063.94 ms | +839.32 ms (+68.5%) |

O recall@10 confirma a primeira observação — a fusão de 3 listas não muda
*quais* ids voltam vs. brute-force, só a ordem entre candidatos já relevantes,
exatamente o contrato do ADR. Mas a latência no `agent-mem-100k` estoura o
limiar de regressão do §5 do BENCHMARKS.md (p99 não pode regredir > 15%
vs. baseline) por larga margem: +68.5%, mais de 4× o limiar. O custo é o
esperado pela análise (recarregar `created_at` de até `2·limit` candidatos e
ordenar), mas nesta escala ele não é desprezível — em 10k passa despercebido
(+0.04%), em 100k mais que dobra o p99. **Decisão: `recency` fica opt-in,
default desligado.** Quem quiser o desempate por frescor liga explicitamente
sabendo do custo em bases grandes; o caminho default (maioria dos recalls)
não paga essa latência. Reavaliar se o `remember` de `created_at` ganhar um
índice dedicado que evite a releitura de record por candidato.

## Alternativas rejeitadas

- **Decaimento temporal no score** (`score × e^(−Δt/τ)` ou pesos por idade):
  introduz exatamente a constante calibrável que o ADR 0005 rejeita — e num
  arquivo de memória que vive anos, qualquer `τ` fixo está errado para algum
  usuário. Rank-only não tem esse parâmetro.
- **Lista de recência sobre o corpus inteiro** (mais-novos-primeiro, global):
  injetaria candidatos irrelevantes só por serem novos, furaria os filtros da
  query (teria que re-filtrar o arquivo inteiro por busca) e violaria a borda
  "nunca reintroduz um excluído" — a união dos candidatos de conteúdo já vem
  filtrada de graça.
- **Timestamp como desempate lexicográfico** (só quando o score fundido empata
  exatamente): empate exato de float é raro fora de simetrias artificiais —
  não realiza o "pende para o mais novo" em matches *comparáveis* (rank 0 vs.
  rank 1), que é o caso real de fato+correção; a spec pede que um match forte
  possa perder para outro hit de conteúdo comparável mais novo.

## Consequências

- `Query::recency(bool)`; MCP `recall` aceita `recency` (boolean, erro tipado
  se não-boolean); CLI ganha a flag `--recency`. Default desligado nos três —
  quem quer o desempate por frescor pede explicitamente.
- Casos de ouro nos testes E2E (`crates/embedmind-core/tests/recall.rs`):
  fato+correção em empate genuíno de conteúdo → a correção vem primeiro;
  match forte antigo vs. novidade fraca → o antigo segue primeiro; recência
  respeita escopo (não puxa memória mais nova de outro projeto).
- Reafirmação idêntica palavra-a-palavra NÃO desloca a original: com texto
  idêntico os índices de conteúdo ranqueiam a primeira gravada em rank 0 nas
  duas listas — é o caso "match forte não é derrubado", não o caso "empate";
  teste dedicado trava esse comportamento observado.
- A lista extra custa ≤ `2·limit` releituras de record por query (ids já no
  page cache pela própria busca) + um sort — medido acima, sem página nova,
  sem mudança de formato.
