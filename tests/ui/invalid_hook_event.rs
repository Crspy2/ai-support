use extensions_macros::{extension, hook};

struct MyExt;

#[extension]
impl MyExt {
    #[hook(event = "issue::typo")]
    async fn on_event(&self, _: ()) -> anyhow::Result<()> {
        Ok(())
    }
}

fn main() {}
