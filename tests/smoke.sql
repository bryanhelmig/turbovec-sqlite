.bail on

create table assertions(ok integer not null check(ok));

insert into assertions values (turbovec_version() = '0.1.2');
insert into assertions values (length(turbovec_f32('[1, 2, 3]')) = 12);

create table documents(
  id integer primary key,
  body text not null,
  embedding blob not null
);

insert into documents values
  (1, 'east', turbovec_f32('[1, 0, 0, 0, 0, 0, 0, 0]')),
  (2, 'north', turbovec_f32('[0, 1, 0, 0, 0, 0, 0, 0]')),
  (3, 'near east', turbovec_f32('[0.9, 0.1, 0, 0, 0, 0, 0, 0]'));

create table vector_indexes(
  name text primary key,
  payload blob not null
);

insert into vector_indexes(name, payload)
select 'documents', turbovec_build(8, 4, id, embedding)
from documents;

insert into assertions
select turbovec_len(payload) = 3
   and turbovec_dimensions(payload) = 8
   and turbovec_bit_width(payload) = 4
from vector_indexes;

insert into assertions
select group_concat(id, ',') = '1,3'
from (
  select id
  from turbovec_knn(
    (select payload from vector_indexes where name = 'documents'),
    '[1, 0, 0, 0, 0, 0, 0, 0]',
    2
  )
  order by score desc
);

update vector_indexes
set payload = turbovec_add(payload, 4, '[-1, 0, 0, 0, 0, 0, 0, 0]')
where name = 'documents';
insert into assertions
select turbovec_len(payload) = 4 from vector_indexes;

update vector_indexes
set payload = turbovec_remove(payload, 2)
where name = 'documents';
insert into assertions
select turbovec_len(payload) = 3 from vector_indexes;

-- Recommended path: a writable, contentless virtual table whose serialized
-- index is stored in fixed-size SQLite shadow-table chunks.
create virtual table document_vectors using turbovec0(
  dimensions=8,
  bit_width=4
);

insert into document_vectors(rowid, embedding)
select id, embedding from documents;

insert into assertions values ((select count(*) from document_vectors) = 3);
insert into assertions values ((select generation from document_vectors_meta) = 1);
insert into assertions
select count(*) = 2
from pragma_table_list
where name in ('document_vectors_meta', 'document_vectors_chunks')
  and type = 'shadow';

insert into assertions
select group_concat(rowid, ',') = '1,3'
from (
  select rowid
  from document_vectors
  where embedding match '[1, 0, 0, 0, 0, 0, 0, 0]'
  order by score desc
  limit 2
);

insert into assertions
select group_concat(rowid, ',') = '3'
from (
  select rowid
  from document_vectors
  where embedding match '[1, 0, 0, 0, 0, 0, 0, 0]'
  order by score desc
  limit 1 offset 1
);

begin immediate;
insert into document_vectors(rowid, embedding)
values (4, '[-1, 0, 0, 0, 0, 0, 0, 0]');
insert into assertions values ((select count(*) from document_vectors) = 4);
rollback;
insert into assertions values ((select count(*) from document_vectors) = 3);
insert into assertions values ((select generation from document_vectors_meta) = 1);

begin immediate;
insert into document_vectors(rowid, embedding)
values (4, '[-1, 0, 0, 0, 0, 0, 0, 0]');
savepoint after_four;
insert into document_vectors(rowid, embedding)
values (5, '[0, 0, 1, 0, 0, 0, 0, 0]');
rollback to after_four;
release after_four;
commit;
insert into assertions values ((select count(*) from document_vectors) = 4);
insert into assertions values ((select generation from document_vectors_meta) = 2);

delete from document_vectors where rowid = 2;
insert into assertions values ((select count(*) from document_vectors) = 3);
insert into assertions values ((select generation from document_vectors_meta) = 3);

insert or ignore into document_vectors(rowid, embedding)
values (1, '[0, 1, 0, 0, 0, 0, 0, 0]');
insert into assertions
select rowid = 1
from document_vectors
where embedding match '[1, 0, 0, 0, 0, 0, 0, 0]' and k = 1
order by score desc;

insert or replace into document_vectors(rowid, embedding)
values (1, '[0, 1, 0, 0, 0, 0, 0, 0]');
insert into assertions
select rowid = 1
from document_vectors
where embedding match '[0, 1, 0, 0, 0, 0, 0, 0]' and k = 1
order by score desc;

alter table document_vectors rename to renamed_vectors;
insert into assertions values ((select count(*) from renamed_vectors) = 3);
insert into assertions
select count(*) = 2
from pragma_table_list
where name in ('renamed_vectors_meta', 'renamed_vectors_chunks')
  and type = 'shadow';
insert into assertions
select integrity_check = 'ok' from pragma_integrity_check;

create virtual table disposable_vectors using turbovec0(dimensions=8, bit_width=4);
drop table disposable_vectors;
insert into assertions
select count(*) = 0
from pragma_table_list
where name in ('disposable_vectors_meta', 'disposable_vectors_chunks');

begin immediate;
update vector_indexes
set payload = turbovec_remove(payload, 1)
where name = 'documents';
insert into assertions
select turbovec_len(payload) = 2 from vector_indexes;
rollback;

insert into assertions
select turbovec_len(payload) = 3 from vector_indexes;
