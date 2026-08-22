create table documents(id integer primary key, body text not null);
insert into documents values
  (1, 'east'),
  (2, 'north'),
  (3, 'near east');

create virtual table document_vectors using turbovec0(
  dimensions=8,
  bit_width=4
);
insert into document_vectors(rowid, embedding) values
  (1, '[1, 0, 0, 0, 0, 0, 0, 0]'),
  (2, '[0, 1, 0, 0, 0, 0, 0, 0]'),
  (3, '[0.9, 0.1, 0, 0, 0, 0, 0, 0]');

.headers on
.mode box
with matches as (
  select rowid, score
  from document_vectors
  where embedding match '[1, 0, 0, 0, 0, 0, 0, 0]'
  order by score desc
  limit 2
)
select d.id, d.body, round(matches.score, 4) as score
from matches
join documents as d on d.id = matches.rowid
order by matches.score desc;
