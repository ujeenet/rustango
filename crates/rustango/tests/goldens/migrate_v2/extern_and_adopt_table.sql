-- postgres
[op 2] tx
  CREATE TABLE "s3_child" ("id" BIGINT NOT NULL, "ext_id" BIGINT NOT NULL, PRIMARY KEY ("id"), CONSTRAINT "s3_child_ext_fk" FOREIGN KEY ("ext_id") REFERENCES "s3_ext" ("id"));
  CREATE INDEX "s3_child_ext_idx" ON "s3_child" ("ext_id");
-- mysql
[op 2] autocommit
  CREATE TABLE `s3_child` (`id` BIGINT NOT NULL, `ext_id` BIGINT NOT NULL, PRIMARY KEY (`id`), CONSTRAINT `s3_child_ext_fk` FOREIGN KEY (`ext_id`) REFERENCES `s3_ext` (`id`), INDEX `s3_child_ext_idx` (`ext_id`));
-- sqlite
[op 2] tx
  CREATE TABLE "s3_child" ("id" INTEGER NOT NULL, "ext_id" INTEGER NOT NULL, PRIMARY KEY ("id"), CONSTRAINT "s3_child_ext_fk" FOREIGN KEY ("ext_id") REFERENCES "s3_ext" ("id"));
  CREATE INDEX "s3_child_ext_idx" ON "s3_child" ("ext_id");
