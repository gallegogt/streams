use iota_streams::{
    app::message::HasLink as _,
    app_channels::{
        api::{
            tangle::{
                Transport,
                Author,
                Subscriber
            },
        },
    },
};

pub async fn s_fetch_next_messages<T: Transport>(subscriber: &mut Subscriber<T>)
{
    let mut exists = true;

    while exists {
        let msgs = subscriber.fetch_next_msgs();
        exists = false;

        for msg in msgs.await {
            println!("Message exists at {}... ", &msg.link.rel());
            exists = true;
        }

        if !exists {
            println!("No more messages in sequence.");
        }
    }
}

pub async fn a_fetch_next_messages<T: Transport>(author: &mut Author<T>)
{
    let mut exists = true;

    while exists {
        let msgs = author.fetch_next_msgs();
        exists = false;

        for msg in msgs.await {
            println!("Message exists at {}... ", &msg.link.rel());
            exists = true;
        }

        if !exists {
            println!("No more messages in sequence.");
        }
    }

}