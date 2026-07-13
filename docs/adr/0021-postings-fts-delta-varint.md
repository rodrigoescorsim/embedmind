# ADR 0021 — Postings FTS comprimidas com delta+varint (`format_version` 4)

**Status:** Aceito (jul/2026). Executa o passo 2 da ordem de risco do
[ADR 0017](0017-otimizacao-do-full-text-escopo-e-metodo.md) §3 — a primeira
otimização da fase FT que muda a codificação em disco, usando o bump de
`format_version` que o ADR 0017 §2 já liberou (decisão do founder,
2026-07-11), desde que aditivo na prática: arquivo antigo continua legível.

## Contexto

O gate de entrada da S26 era o benchmark oficial pós-FT2: se o NFR
`recall p99 @ 100k < 50 ms` já tivesse fechado com a early termination
(ADR 0018), esta otimização seria adiada. Não fechou. A rodada oficial
`benches/run_all.sh --full` de 2026-07-12 (`benches/results/latest.json`,
mesma rodada que fechou o RSS no ADR 0020) mediu **recall p99 @ 100k =
956,80 ms** — ~19x acima do alvo. A S26 prossegue.

Onde esta otimização ataca: depois da FT2, a passada de bounds de
`fts::search` continua percorrendo **todas** as postings dos termos casados
(~366 mil por query @ 100k, medição da FT1 no ADR 0017). Cada entrada custa
20 bytes fixos em disco (`record_id` ULID de 16 bytes + `term_freq` u32,
FORMAT.md §11) — bytes que passam pelo pager, pelas cadeias `FTS_POSTINGS`
e pelo decode a cada lookup. Reduzir bytes por entrada reduz I/O de página e
trabalho de decode sem mudar semântica alguma.

Honestidade sobre o teto do ganho: a FT1 mediu o lookup+decode de postings em
1,2% do tempo **do scan exaustivo pré-FT2**; a FT2 eliminou a maior parte dos
outros 98,8%, então a fração das postings no tempo restante é hoje maior —
mas não foi re-perfilada isoladamente. E como os 80 bits baixos de um ULID
são aleatórios, o delta entre ids consecutivos raramente cabe em poucos
bytes: a compressão típica esperada é ~30–40% por entrada (20 → ~12–14
bytes), não a ordem de magnitude que delta+varint entrega sobre doc-ids
densos. Esta é a parte 1 da S26; se o NFR seguir aberto, o corte assintótico
de verdade é o passo 3 do ADR 0017 §3 (skip lists), que fica para decisão
posterior com o dado novo em mãos.

## Decisão

1. **Codificação.** A partir de `format_version` **4**, o corpo de postings
   de um termo passa de entradas fixas de 20 bytes para: delta do
   `record_id` (tratado como u128) em relação à entrada anterior, codificado
   como **varint LEB128** (mínimo por construção — determinismo G3), seguido
   de `term_freq` também como varint. O primeiro delta é o valor bruto do
   id; como a lista é ordenada estritamente ascendente (FORMAT.md §11), todo
   delta subsequente é ≥ 1. O prefixo `doc_freq` (u32) e o framing do
   dicionário/cadeias `FTS_POSTINGS` não mudam. Spec normativa: FORMAT.md
   §2 e §11.

2. **Seleção de layout pela versão do arquivo, nunca por corpo.** O layout é
   função do `format_version` do header — um arquivo nunca mistura layouts.
   Um build v4 abrindo arquivo v≤3 **lê e escreve** no layout fixo antigo
   (degrada em tamanho/latência, nunca em corretude nem em erro), então o
   arquivo continua legível pelo build que o criou. Leitor v3 diante de
   arquivo v4 recusa com erro claro (política G4). Migração = o rebuild por
   cópia que já existe: `vacuum` reconstrói o índice via `index_document`
   num arquivo novo (v4), re-codificando as postings.

3. **Decoder defensivo nos dois layouts.** O decoder novo rejeita: varint
   truncado, varint com mais de 19 bytes, bits de dado além de 128 bits,
   delta zero após a primeira entrada (id duplicado/não ordenado), soma de
   id além de `u128::MAX`, `term_freq` 0 ou além de u32, e `doc_freq`
   hostil (validado contra o tamanho do corpo antes de alocar). O fuzz body
   `fuzz_fts_page` decodifica cada input sob **ambos** os layouts, no mesmo
   commit da mudança de formato (regra do 04-agents.md); o corpus ganhou
   seeds v4 (`seed-type-08-v4`/`seed-type-09-v4`) e mantém os seeds antigos
   para o branch fixo.

4. **Nenhum resultado de busca muda.** Encode/decode são bijeções sobre a
   mesma `Postings` em memória; BM25, early termination (ADR 0018) e RRF
   não são tocados. A equivalência continua garantida pelo oráculo
   `search_profiled` e pelos testes de round-trip nos dois layouts.

## Alternativas rejeitadas

- **Tag de layout por corpo de postings** (byte de versão em cada corpo):
  permitiria upgrade incremental in-place, mas cria arquivos de estado misto
  — pior de auditar, pior de fuzzar, e o ganho é nulo dado que `vacuum` já é
  o caminho de migração por cópia (ADR 0003). Rejeitado.
- **Upgrade silencioso do `format_version` na primeira escrita** num arquivo
  v3: quebraria o build antigo que ainda lê aquele arquivo (G4 o mandaria
  recusar) sem o usuário ter pedido nada. Upgrade só por `vacuum`, explícito.
- **Compressão por bloco (bit-packing/PFOR) ou dos 80 bits aleatórios do
  ULID**: ganho extra real, mas estrutura de página nova e mais estado — é
  território do passo 3 (skip lists) do ADR 0017 §3, a avaliar com medição
  depois desta entrega, não junto.
- **Varint também no `doc_freq`**: economizaria ~3 bytes por termo, mas
  perderia a validação barata de count-contra-buffer antes de alocar
  (regra de fuzz, TESTING.md §3) por um ganho marginal. Mantido u32.

## Consequências

- `FORMAT_VERSION` = 4; arquivos novos nascem v4. Arquivos v≤3 seguem
  plenamente funcionais neste build, no layout deles (round-trip coberto por
  teste com arquivo v3 genuíno, criado pelo caminho de escrita antigo que
  este build preserva verbatim).
- `PagerOptions.format_version` (default `FORMAT_VERSION`) permite criar
  arquivo de versão antiga suportada — existe para os testes de
  compatibilidade entre versões; `Pager::open` ignora a opção e usa a versão
  do header.
- A validação do impacto no NFR `recall p99 @ 100k` fica para a task de
  fechamento da fase FT (benchmark completo é execução longa, fora desta
  sessão) — mesma régua de sempre: `benches/run_all.sh --full`.
- O corpus sintético de benchmark tem ULIDs cunhados em sequência rápida;
  corpora reais com ingestão espaçada terão deltas maiores (mais bytes por
  varint). O número honesto de compressão sai da medição, não desta previsão.
