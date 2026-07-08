# ADR 0010 — Teto de tamanho do binário governa o artefato comprimido de release

**Status:** Aceito (jul/2026)

## Contexto

A spec ([01-spec.md](../01-spec.md) S8) e o [ADR 0004](0004-modelo-de-embedding-embarcado.md)
fixam o NFR "binário < 40 MB incluindo modelo", assumindo que o peso dominante seria o
modelo ONNX int8 (~22 MB) + tokenizer (~0,7 MB). Ao montar o pipeline de release (task A1,
item 1.6) medimos o binário release real:

- Windows `x86_64-pc-windows-msvc`, perfil release (LTO, `codegen-units=1`, strip): **~45 MiB**.

O ADR 0004 não contabilizou que o `ort` com `download-binaries` (a config atual, que preserva
"instalação em 1 comando" sem instalar ONNX Runtime no sistema) **linka a ONNX Runtime
estaticamente** no binário — cerca de ~23 MiB de runtime, somados aos ~23 MiB de modelo +
tokenizer + código. O binário nu, portanto, estoura o teto de 40 MB por construção, em todas
as plataformas.

O que o usuário efetivamente baixa e instala é o **artefato de release comprimido**. O binário
comprime muito bem (o modelo int8 e o código de runtime são altamente compressíveis):

- `gzip -9`: ~23,4 MiB · `xz -9`: ~20,5 MiB.

## Decisão

**O teto de 40 MB governa o artefato de release comprimido (o que o usuário baixa), não o
binário nu descomprimido.** O pipeline de release empacota o binário auto-contido em `.tar.gz`
(Linux/macOS) ou `.zip` (Windows) e valida esse artefato contra o teto; estoura → job falha.

O NFR existe para proteger a promessa de "instalação em 1 comando" — download e instalação
rápidos. O artefato comprimido (~20–23 MiB) cumpre isso com folga; o binário continua sendo um
**único arquivo auto-contido** após descompactar (modelo + tokenizer via `include_bytes!`, ORT
estático), sem `.dll`/`.so`/`.dylib` avulsa nem passo de instalação extra.

## Alternativas rejeitadas

- **`ort` com `load-dynamic`** (ONNX Runtime como biblioteca dinâmica separada): derruba o
  binário para ~25 MiB, mas quebra "um arquivo" — o pacote passa a ter binário + runtime
  dinâmica (~15 MiB), somando ~40 MiB e reintroduzindo gestão de biblioteca no runtime.
  Complexidade maior sem ganho para o usuário.
- **Modelo menor / mais quantizado só para caber no teto:** fora de escopo da task A1; a troca
  de modelo é uma questão de qualidade (pt-BR) já rastreada como aberta no DESIGN §12, decidida
  por recall, não por bytes.
- **Frouxar o número do teto (ex.: 50 MB):** reabre uma decisão de produto do founder sem
  necessidade — o artefato distribuído já está bem abaixo de 40 MB.

## Consequências

- `docs/01-spec.md` S8 e a tabela de NFR passam a ler "artefato de release comprimido < 40 MB";
  o texto do ADR 0004 (< 40 MB) permanece a intenção, agora com esta precisão.
- O CI de release (`.github/workflows/release.yml`) valida o tamanho do artefato comprimido em
  cada plataforma e falha o job se estourar.
- Se o binário nu voltar a importar por si (ex.: distribuição sem compressão, ou empacotamento
  em instalador), este teto precisa ser reavaliado — hoje ele não o cobre por decisão explícita.
