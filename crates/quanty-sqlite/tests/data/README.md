# Test data

`chinook.sqlite` is the Chinook sample database, byte for byte as published
in release v1.4.5 of https://github.com/lerocha/chinook-database, file
`Chinook_Sqlite.sqlite`. It is MIT licensed, copyright Luis Rocha; the
license text sits next to it in LICENSE.chinook.txt.

It is checked in rather than downloaded because a test that needs the
network is not a test. One megabyte in git is a fair price for having the
acceptance corpus present in every checkout and every CI run.

Why this file specifically. It is a real database written by a real SQLite
(3.36.0), not something we generated to match our own reader, and it covers
the parts of the format that are easy to get wrong:

- 11 tables and 11 indexes, so `sqlite_master` has both kinds in it
- 15607 rows, enough that a wrong page traversal shows up as a wrong count
- interior and leaf pages, since the larger tables do not fit one page
- a composite primary key on PlaylistTrack, which is a table with a
  separate index rather than a rowid alias
- nullable columns with real NULLs in them
- declared types the SQL standard does not have (NVARCHAR, NUMERIC,
  DATETIME), which is where type mapping has to make a decision
- text values outside ASCII (accented names in several tables)
- a non-empty freelist (199 free pages), so a reader that walks the file
  linearly instead of following the b-trees gets caught
- page size 1024 rather than the 4096 default, so a hard coded page size
  gets caught

The header, for reference when reading test expectations: page size 1024,
1042 pages, reserved space 0, text encoding UTF-8, schema format 4, first
freelist trunk page 8.
