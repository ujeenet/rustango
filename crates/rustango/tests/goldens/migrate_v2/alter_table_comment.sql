-- postgres
[op 0] tx
  COMMENT ON TABLE "s3_parent" IS 'people';
-- mysql
[op 0] autocommit
  ALTER TABLE `s3_parent` COMMENT='people';
-- sqlite
