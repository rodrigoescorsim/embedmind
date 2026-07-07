# ADR 0006 — Single-writer / multi-reader, sem MVCC

**Status:** Aceito (jul/2026)

## Contexto

O caso de uso real é um agente (ou o CLI) por arquivo de memória, na máquina do usuário.
Concorrência de escrita multi-processo é cenário raro, mas o comportamento quando
acontece precisa ser claro e seguro.

## Decisão

- **Em processo:** escritas serializadas por lock interno; leituras via snapshot leve
  (page cache copy-on-write) — leitores nunca bloqueiam nem veem estado parcial.
- **Entre processos:** lock de arquivo advisory (`LockFileEx` no Windows); um segundo
  escritor recebe erro claro imediatamente; leitores concorrentes são permitidos.
- MVCC completo é **não-objetivo** da v0.x.

## Alternativas rejeitadas

- **MVCC multi-versão:** semanas de complexidade em storage para um cenário que o
  produto não tem (não é um banco multi-tenant); ampliaria a superfície de bugs de
  corrupção — o risco nº 1.
- **Fila multi-processo (um daemon coordenador):** reintroduz o "servidor para babysit"
  que o produto existe para eliminar.

## Consequências

- Modelo mental simples; erro de segundo escritor é explícito, não deadlock nem corrupção.
- Memória compartilhada de equipe (multi-escritor real) fica corretamente empurrada para o premium de sync (M4+), onde a coordenação é por replicação, não por lock de arquivo.
