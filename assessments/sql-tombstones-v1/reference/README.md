Rank visible events per resource before dropping deleted winners. With live v1
and deleted v2, filtering deletions first incorrectly returns v1. Higher event_id
breaks equal-version ties. Reference fixture; private controls are run in Docker.
