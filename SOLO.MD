# Plan för Solo Mode med Hover-to-Expand

## 1. Syfte
Att förbättra Solo-läget så att användaren snabbt kan titta i andra (minimerade) fönster genom att bara föra muspekaren över deras namnlister, utan att behöva lämna Solo-läget permanent.

## 2. Funktionskrav
- [x] **Solo Mode**: Fungerar som idag (ett fönster visas, resten minimeras).
- [x] **Hover Expand**: Om muspekaren förs över namnlisten på ett minimerat fönster ska det expandera temporärt.
- [x] **Auto Collapse**: När muspekaren lämnar det temporärt expanderade fönstret ska det minimeras igen.
- [x] **Villkor**: Kollaps ska endast ske om fönstret var minimerat när hover-sekvensen började.
- [x] **Ingen permanent ändring**: Att titta på ett fönster via hover ska inte ändra fönstrets "riktiga" status i Solo-läget.

## 3. Tekniska ändringar

### A. Protokoll (`src/protocol.rs`)
- [x] Lägg till `TemporaryExpand { window_id: usize }` i `ClientMessage`.
- [x] Lägg till `TemporaryCollapse { window_id: usize }` i `ClientMessage`.
- [x] Inkludera `solo_mode_active: bool` och `solo_origin_id: Option<usize>` i `ServerMessage::FullSync` och `Welcome` så att klienten vet när solo-logik ska appliceras.

### B. Server (`src/server.rs`)
- [x] Implementera hantering av `TemporaryExpand`:
    - Sätt fönstrets `minimized = false`.
    - Skicka `WindowUpdate` till alla klienter.
- [x] Implementera hantering av `TemporaryCollapse`:
    - Sätt fönstrets `minimized = true`.
    - Skicka `WindowUpdate` till alla klienter.
- [x] Säkerställ att `ToggleSolo(off)` fortfarande återställer alla fönster korrekt oavsett temporära expansions-statusar.

### C. Klient (`src/client.rs`)
- [x] Uppdatera `Client`-structen för att hålla koll på `solo_mode_origin` och `temporarily_expanded_id`.
- [x] Uppdatera `handle_mouse` för att lyssna på `MouseEventKind::Moved`.
- [x] **Logik vid Moved**:
    1. Om Solo-läge är aktivt:
    2. Om musen träffar `HitTarget::WindowTitle(id)` och `id != solo_origin` och fönstret är minimerat:
        - Skicka `TemporaryExpand(id)`.
        - Sätt `self.temporarily_expanded_id = Some(id)`.
    3. Om `self.temporarily_expanded_id` är satt till `Some(tid)` och musen INTE längre är över fönster `tid`:
        - Skicka `TemporaryCollapse(tid)`.
        - Sätt `self.temporarily_expanded_id = None`.

## 4. Verifieringssteg (Kompilerar och logiskt verifierat)
1. Starta Solo-läge på fönster A.
2. Bekräfta att fönster B och C minimeras.
3. För musen över namnlisten för fönster B.
4. Bekräfta att fönster B expanderar och visar sitt innehåll.
5. Flytta musen utanför fönster B.
6. Bekräfta att fönster B minimeras igen.
7. Stäng av Solo-läge och bekräfta att alla fönster återgår till normal status.
