---
default: major
---

# `zaino-traffic`: `Answered::from` removed; the answering send is `Answered::ticket`

`Answered::from` removed (it always equalled the ticket's member); `Answered { value, ticket }`, and `report` takes the ticket.
